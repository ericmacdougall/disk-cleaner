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
    decode_seed(z, std::marker::PhantomData::<T>)
}

/// As [`decode`], through a seed that can consume the value as it parses (the index loader
/// puts each chunk straight into the map instead of building the object first).
pub fn decode_seed<S, V>(z: &[u8], seed: S) -> Result<V>
where
    S: for<'de> serde::de::DeserializeSeed<'de, Value = V>,
{
    // the broken-restore-guards variant lifts the object limit (failing-first evidence only)
    let limit = if cfg!(feature = "broken-restore-guards") {
        8 * OBJECT_JSON_MAX
    } else {
        OBJECT_JSON_MAX
    };
    if cfg!(feature = "broken-mem-plan") {
        let json = bounded_zstd(z, limit)?;
        let mut de = serde_json::Deserializer::from_slice(&json);
        return seed
            .deserialize(&mut de)
            .and_then(|v| de.end().map(|()| v))
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
    let v = seed.deserialize(&mut de).and_then(|v| de.end().map(|()| v));
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
            return Err(self.refusal());
        }
        Ok(())
    }
    /// The error for a charge that does not fit.
    pub fn refusal(&self) -> Error {
        Error::Budget(format!(
            "{} needs more than its {} share of --mem-budget",
            self.what,
            crate::util::human(self.limit)
        ))
    }
    /// What is left of the share.
    pub fn left(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }
    /// Whether `n` more bytes fit, without charging them.
    pub fn fits(&self, n: u64) -> bool {
        self.used.saturating_add(n) <= self.limit || cfg!(feature = "broken-restore-guards")
    }
}

/// Bytes of one committed chunk-map entry (`ChunkId` and `Loc`), in a sorted vector.
pub const MAP_ENTRY: u64 = 40;

/// What one pack held by a chunk map costs: its `ObjRef` in the pack list (two strings and a
/// size) and its key again in the pack lookup table, with allocator and table slack.
pub fn pack_cost(p: &ObjRef) -> u64 {
    2 * p.key.len() as u64 + p.hash.len() as u64 + 200
}

/// The cost of a typical pack (a 81-byte key and a 40-character sha1).
pub const PACK_COST: u64 = 2 * 81 + 40 + 200;

/// The most chunks a map with share `bytes` holds when packs carry `per_pack` chunks each:
/// each pack costs its place in the map and its entry in the pack listing.
pub fn map_fit(bytes: u64, per_pack: u64) -> usize {
    let left = bytes.saturating_sub(slots_bytes(bytes)) as u128;
    (left * per_pack as u128 / (MAP_ENTRY * per_pack + 2 * PACK_COST) as u128) as usize
}
/// Bytes of one bucket of the hot table (`ChunkId`, its verified flag, one control byte).
pub const HOT_BUCKET: u64 = 34;
/// The largest share the hot table takes.
pub const HOT_MAX: u64 = 32 * MIB;

fn table_cap(b: u64) -> u64 {
    if b < 8 {
        b - 1
    } else {
        b / 8 * 7
    }
}

/// The bucket count of a std `HashMap` or `HashSet` holding `n` entries that were inserted one
/// at a time: hashbrown starts at 4 buckets, fills at most 7/8 of them (all but one below 8),
/// and doubles when full. `table_model_matches_std` checks this against `HashMap::capacity`.
pub fn table_buckets(n: usize) -> u64 {
    let mut b = 4u64;
    while table_cap(b) < n as u64 {
        b *= 2;
    }
    b
}

/// The hot table a backup's index share `i` sets aside, allocated once at start: its bucket
/// count and bytes. It takes at most a quarter of the share and `HOT_MAX`; the rest holds the
/// committed map.
pub fn hot_table(i: u64) -> (u64, u64) {
    let room = (i / 4).min(HOT_MAX);
    let bytes = |b: u64| b * HOT_BUCKET + 16;
    let mut b = 8u64;
    while bytes(b * 2) <= room {
        b *= 2;
    }
    (b, bytes(b))
}

/// The new chunks a backup holds at once before readers wait for a commit (a pass boundary):
/// half the hot table's capacity. Removals leave tombstones; at most half full, a table that
/// runs out of free slots rehashes in place instead of allocating a larger one.
pub fn hot_room(i: u64) -> usize {
    (table_cap(hot_table(i).0) / 2) as usize
}

/// The most committed chunk-map entries `bytes` hold (the prefix table aside).
pub fn chunk_map_fit(bytes: u64) -> usize {
    (bytes.saturating_sub(slots_bytes(bytes)) / MAP_ENTRY) as usize
}

/// The most committed chunks a backup with index share `i` holds: the share less its hot table.
pub fn backup_map_fit(i: u64) -> usize {
    chunk_map_fit(i.saturating_sub(hot_table(i).1))
}

/// The largest index share any `--mem-budget` gives (Revision 6: min(256 MiB, budget / 8)).
pub const INDEX_SHARE_MAX: u64 = 256 * MIB;

/// The smallest `--mem-budget` (whole MiB) whose index share holds `need` bytes of chunk map,
/// besides the hot table when `backup`. None when no budget does.
pub fn budget_for_map(need: u64, backup: bool) -> Option<u64> {
    let fits = |m: u64| {
        let i = (256 * MIB).min(m * MIB / 8);
        let hot = if backup { hot_table(i).1 } else { 0 };
        i >= hot + need
    };
    (1..=8 * INDEX_SHARE_MAX / MIB)
        .find(|&m| fits(m))
        .map(|m| m * MIB)
}

/// The refusal when the chunk map outgrows its share: how much it needs, estimated from the
/// part of the index loaded so far (`loaded` entries from `done` of `total` listed bytes), and
/// which `--mem-budget` would hold it, if any does.
pub fn over_index_share(loaded: usize, done: u64, total: u64, limit: u64, backup: bool) -> Error {
    let est = (loaded as u128 * total.max(1) as u128 / done.max(1) as u128) as usize;
    let est = est.max(loaded + 1);
    let need = est as u64 * MAP_ENTRY + slots_bytes(INDEX_SHARE_MAX);
    let h = crate::util::human;
    let advice = match budget_for_map(need, backup) {
        Some(m) => format!("a --mem-budget of {} or more holds it", h(m)),
        None => format!(
            "no --mem-budget holds it: the in-memory index holds at most {} chunks",
            if backup {
                backup_map_fit(INDEX_SHARE_MAX)
            } else {
                chunk_map_fit(INDEX_SHARE_MAX)
            }
        ),
    };
    Error::Budget(format!(
        "the chunk map needs about {} for about {est} chunks, more than its {} share of \
         --mem-budget; {advice}",
        h(need),
        h(limit)
    ))
}

// ---------------------------------------------------------------------------------------------
// Repository access

#[derive(Clone, Copy, Debug)]
pub struct Loc {
    pub pack: u32,
    pub len: u32,
}

/// The prefix bits of a [`ChunkMap`] whose share is `limit` bytes: up to 16, with the prefix
/// table (4 bytes a slot) at most 1/256 of the share.
fn slot_bits(limit: u64) -> u32 {
    (limit / 1024).max(1).ilog2().min(16)
}

/// Bytes the prefix table of a map with share `limit` takes.
pub fn slots_bytes(limit: u64) -> u64 {
    ((1u64 << slot_bits(limit)) + 1) * 4
}

/// The union of committed chunk maps whose packs are still listed with the recorded size and
/// hash: a vector of (id, location) sorted by id, with a prefix table over the first bits of
/// the id. 40 bytes per entry, and it never rehashes: an index loaded at start is reserved
/// once, and a backup merges each commit's chunks in place (see [`ChunkMap::merge`]).
pub struct ChunkMap {
    pub packs: Vec<ObjRef>,
    pack_ix: HashMap<String, u32>,
    /// Sorted by id without duplicates, except while loading (`sorted == false`).
    entries: Vec<(ChunkId, Loc)>,
    sorted: bool,
    bits: u32,
    /// `slots[p]..slots[p + 1]` is the range of `entries` whose id starts with prefix `p`.
    slots: Vec<u32>,
}

impl ChunkMap {
    /// An empty, sorted map, charged for its prefix table.
    pub fn new(meter: &mut Meter) -> Result<ChunkMap> {
        meter.charge(slots_bytes(meter.limit))?;
        let mut m = ChunkMap {
            packs: Vec::new(),
            pack_ix: HashMap::new(),
            entries: Vec::new(),
            sorted: true,
            bits: slot_bits(meter.limit),
            slots: Vec::new(),
        };
        m.rebuild_slots();
        Ok(m)
    }
    fn prefix(&self, id: &ChunkId) -> usize {
        ((id.0[0] as usize) << 8 | id.0[1] as usize) >> (16 - self.bits)
    }
    /// Append a chunk while loading, charged first (40 bytes); `finish` sorts.
    pub fn push_charged(&mut self, id: ChunkId, loc: Loc, meter: &mut Meter) -> Result<()> {
        if !meter.fits(MAP_ENTRY) {
            return Err(meter.refusal());
        }
        self.room_for(1, meter)?;
        meter.charge(MAP_ENTRY)?;
        self.entries.push((id, loc));
        self.sorted = false;
        Ok(())
    }
    /// Reserve, once, room for every entry the rest of `meter` can pay for, so neither loading
    /// nor merging reallocates. Untouched capacity is address space, not memory: its pages
    /// become resident only as entries are written, and each entry is charged as it is.
    /// `hint` (entries expected) caps the reservation when it is smaller.
    pub fn reserve_share(&mut self, meter: &Meter, hint: Option<usize>) {
        let n = (meter.left() / MAP_ENTRY) as usize;
        let n = hint.map_or(n, |h| h.min(n));
        self.entries
            .reserve_exact(n.saturating_sub(self.entries.capacity() - self.entries.len()));
    }
    /// Make room for `more` entries. Past the reservation, duplicates are dropped first; a
    /// reallocation that is still needed copies the map, so the old block (and the new one's
    /// written part) must fit in the share at once: both are charged for the copy.
    fn room_for(&mut self, more: usize, meter: &mut Meter) -> Result<()> {
        if self.entries.capacity() - self.entries.len() >= more {
            return Ok(());
        }
        if !self.sorted {
            self.finish(meter);
            if self.entries.capacity() - self.entries.len() >= more {
                return Ok(());
            }
        }
        let copy = self.entries.len() as u64 * MAP_ENTRY;
        let grow = more
            .max(self.entries.len() / 2)
            .min((meter.left().saturating_sub(copy) / MAP_ENTRY) as usize)
            .max(more);
        if !meter.fits(copy + more as u64 * MAP_ENTRY) {
            return Err(meter.refusal());
        }
        self.entries.reserve_exact(grow);
        Ok(())
    }
    /// Sort, drop duplicate ids, return their charge to `meter`, and build the prefix table.
    pub fn finish(&mut self, meter: &mut Meter) {
        if !self.sorted {
            let before = self.entries.len();
            // in place (a stable sort would allocate half the map again); which of two equal
            // ids is kept does not matter, both name a listed pack holding the same bytes
            self.entries.sort_unstable_by_key(|a| a.0 .0);
            self.entries.dedup_by(|a, b| a.0 == b.0);
            self.sorted = true;
            meter.used -= (before - self.entries.len()) as u64 * MAP_ENTRY;
        }
        self.rebuild_slots();
    }
    fn rebuild_slots(&mut self) {
        let n = (1usize << self.bits) + 1;
        let mut slots = std::mem::take(&mut self.slots);
        slots.clear();
        slots.resize(n, 0);
        for (id, _) in &self.entries {
            slots[self.prefix(id) + 1] += 1;
        }
        for p in 1..n {
            slots[p] += slots[p - 1];
        }
        self.slots = slots;
    }
    /// Merge chunks committed during a backup, charging 40 bytes for each one not already
    /// present. All or nothing: if they do not fit, the map is unchanged and the error says so.
    /// The merge runs from the end in place, so it needs no second copy of the map.
    pub fn merge(&mut self, mut add: Vec<(ChunkId, Loc)>, meter: &mut Meter) -> Result<()> {
        debug_assert!(self.sorted);
        add.sort_unstable_by_key(|a| a.0 .0);
        add.dedup_by(|a, b| a.0 == b.0);
        add.retain(|(id, _)| self.find(id).is_none());
        if add.is_empty() {
            return Ok(());
        }
        if !meter.fits(add.len() as u64 * MAP_ENTRY) {
            return Err(meter.refusal());
        }
        self.room_for(add.len(), meter)?;
        meter.charge(add.len() as u64 * MAP_ENTRY)?;
        let (n, m) = (self.entries.len(), add.len());
        self.entries.extend_from_slice(&add);
        let v = &mut self.entries;
        let (mut i, mut j, mut k) = (n, m, n + m);
        while j > 0 {
            if i > 0 && v[i - 1].0 .0 > add[j - 1].0 .0 {
                v[k - 1] = v[i - 1];
                i -= 1;
            } else {
                v[k - 1] = add[j - 1];
                j -= 1;
            }
            k -= 1;
        }
        // shift each slot boundary by the entries added below it: O(slots + added), not O(map)
        let mut below = 0u32;
        let mut a = 0usize;
        for p in 0..self.slots.len() - 1 {
            self.slots[p] += below;
            while a < m && self.prefix(&add[a].0) == p {
                below += 1;
                a += 1;
            }
        }
        let last = self.slots.len() - 1;
        self.slots[last] += below;
        debug_assert_eq!(self.slots[last] as usize, self.entries.len());
        Ok(())
    }
    fn find(&self, id: &ChunkId) -> Option<Loc> {
        if !self.sorted {
            return self.entries.iter().find(|(x, _)| x == id).map(|(_, l)| *l);
        }
        let p = self.prefix(id);
        let (lo, hi) = (self.slots[p] as usize, self.slots[p + 1] as usize);
        let r = &self.entries[lo..hi];
        r.binary_search_by(|(x, _)| x.0.cmp(&id.0))
            .ok()
            .map(|i| r[i].1)
    }
    pub fn contains(&self, id: &ChunkId) -> bool {
        self.find(id).is_some()
    }
    /// Entries held (before `finish`, duplicates included).
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
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
        self.find(id).map(|l| (&self.packs[l.pack as usize], l.len))
    }
    /// The location of a mapped chunk.
    pub fn loc(&self, id: &ChunkId) -> Option<Loc> {
        self.find(id)
    }
}

/// Loads one `.idx` into a [`ChunkMap`], one pack and one chunk at a time, so an index object is
/// never held decoded: what it costs is what the map keeps, charged as each entry is read.
struct IdxLoad<'a> {
    key: &'a str,
    repo_kind: HashKind,
    listed: &'a HashMap<String, RemoteObject>,
    referenced: &'a mut HashMap<String, ObjRef>,
    map: &'a mut ChunkMap,
    meter: &'a mut Meter,
    v: Option<u32>,
    kind: Option<HashKind>,
    /// the object's packs in order: the map's index of each listed one
    local: Option<Vec<Option<u32>>>,
    chunks_seen: bool,
    entries: usize,
}

impl<'a> IdxLoad<'a> {
    fn new(
        key: &'a str,
        repo_kind: HashKind,
        listed: &'a HashMap<String, RemoteObject>,
        referenced: &'a mut HashMap<String, ObjRef>,
        map: &'a mut ChunkMap,
        meter: &'a mut Meter,
    ) -> IdxLoad<'a> {
        IdxLoad {
            key,
            repo_kind,
            listed,
            referenced,
            map,
            meter,
            v: None,
            kind: None,
            local: None,
            chunks_seen: false,
            entries: 0,
        }
    }
    fn version(&mut self, v: u32) -> Result<()> {
        if v != 1 {
            return Err(Error::Refused(format!(
                "{}: unknown index version",
                self.key
            )));
        }
        self.v = Some(v);
        Ok(())
    }
    fn count(&mut self) -> Result<()> {
        self.entries += 1;
        if self.entries > ENTRIES_MAX {
            return Err(Error::corrupt(format!("{}: too many entries", self.key)));
        }
        Ok(())
    }
    fn pack(&mut self, p: &ObjRef) -> Result<()> {
        self.count()?;
        let Some(kind) = self.kind else {
            return Err(Error::corrupt(format!(
                "{}: packs before hash_kind",
                self.key
            )));
        };
        let ok = self.listed.get(&p.key).is_some_and(|l| {
            l.size == p.size
                && l.hash
                    .as_deref()
                    .is_some_and(|h| kind == self.repo_kind && h.eq_ignore_ascii_case(&p.hash))
        });
        self.meter.charge(8)?;
        let slot = if ok {
            if !self.map.has_pack(&p.key) {
                self.meter.charge(pack_cost(p))?;
            }
            Some(self.map.pack_index(p))
        } else {
            // a reference that does not match the listing: kept to name the damaged pack
            if self.referenced.len() < ENTRIES_MAX && !self.referenced.contains_key(&p.key) {
                self.meter.charge(pack_cost(p))?;
                self.referenced.insert(p.key.clone(), p.clone());
            }
            None
        };
        self.local.get_or_insert_with(Vec::new).push(slot);
        Ok(())
    }
    fn chunk(&mut self, c: &ChunkRef) -> Result<()> {
        self.count()?;
        let Some(local) = &self.local else {
            return Err(Error::corrupt(format!("{}: chunks before packs", self.key)));
        };
        let Some(Some(pi)) = local.get(c.p as usize) else {
            return Ok(());
        };
        let loc = Loc {
            pack: *pi,
            len: c.len,
        };
        self.map.push_charged(c.id, loc, self.meter)
    }
    fn end(&mut self) -> Result<()> {
        if self.v.is_none() || self.kind.is_none() || !self.chunks_seen {
            return Err(Error::corrupt(format!("{}: missing field", self.key)));
        }
        // the per-object slot table is returned to the share
        let n = self.local.take().map_or(0, |l| l.len());
        self.meter.used -= 8 * n as u64;
        Ok(())
    }
}

/// The serde side of [`IdxLoad`]. An error of the load (a refused version, the share spent) is
/// kept in `err` so it reaches the caller as itself, not as a JSON syntax error.
struct IdxSeed<'a, 'e> {
    l: IdxLoad<'a>,
    err: &'e mut Option<Error>,
}

fn stop<E: serde::de::Error>(err: &mut Option<Error>, e: Error) -> E {
    let m = e.to_string();
    *err = Some(e);
    E::custom(m)
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum IdxField {
    V,
    HashKind,
    Packs,
    Chunks,
    #[serde(other)]
    Other,
}

impl<'de> serde::de::DeserializeSeed<'de> for IdxSeed<'_, '_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de> serde::de::Visitor<'de> for IdxSeed<'_, '_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an index object")
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(
        mut self,
        mut a: A,
    ) -> std::result::Result<(), A::Error> {
        while let Some(k) = a.next_key::<IdxField>()? {
            match k {
                IdxField::V => {
                    let v: u32 = a.next_value()?;
                    if let Err(e) = self.l.version(v) {
                        return Err(stop(self.err, e));
                    }
                }
                IdxField::HashKind => self.l.kind = Some(a.next_value()?),
                IdxField::Packs => a.next_value_seed(ListSeed {
                    s: &mut self,
                    packs: true,
                })?,
                IdxField::Chunks => {
                    a.next_value_seed(ListSeed {
                        s: &mut self,
                        packs: false,
                    })?;
                    self.l.chunks_seen = true;
                }
                IdxField::Other => {
                    a.next_value::<serde::de::IgnoredAny>()?;
                }
            }
        }
        if let Err(e) = self.l.end() {
            return Err(stop(self.err, e));
        }
        Ok(())
    }
}

/// The `packs` or `chunks` list of an index object, consumed element by element.
struct ListSeed<'s, 'a, 'e> {
    s: &'s mut IdxSeed<'a, 'e>,
    packs: bool,
}

impl<'de> serde::de::DeserializeSeed<'de> for ListSeed<'_, '_, '_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> serde::de::Visitor<'de> for ListSeed<'_, '_, '_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(if self.packs {
            "a list of packs"
        } else {
            "a list of chunks"
        })
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut a: A,
    ) -> std::result::Result<(), A::Error> {
        let s = self.s;
        if self.packs {
            if s.l.local.is_some() {
                return Err(stop(
                    s.err,
                    Error::corrupt(format!("{}: packs twice", s.l.key)),
                ));
            }
            s.l.local = Some(Vec::new());
            while let Some(p) = a.next_element::<ObjRef>()? {
                if let Err(e) = s.l.pack(&p) {
                    return Err(stop(s.err, e));
                }
            }
        } else {
            while let Some(c) = a.next_element::<ChunkRef>()? {
                if let Err(e) = s.l.chunk(&c) {
                    return Err(stop(s.err, e));
                }
            }
        }
        Ok(())
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

    /// Returns the map, the number of index objects, and every pack an index references that
    /// does not match the listing (missing, or another size or hash), with what it recorded, so
    /// damaged packs can be named. The packs that match are the map's own (`has_pack`); a
    /// listed pack in neither is unreferenced.
    pub fn load_chunk_map(
        &self,
        idx: &[RemoteObject],
        packs: &HashMap<String, RemoteObject>,
        meter: &mut Meter,
        cache: Option<&Cache>,
    ) -> Result<(ChunkMap, usize, HashMap<String, ObjRef>)> {
        self.load_map(idx, packs, meter, cache, false)
    }

    /// As [`Repo::load_chunk_map`]; `backup` says the share also holds a backup's hot table,
    /// which the refusal's advice accounts for.
    pub fn load_map(
        &self,
        idx: &[RemoteObject],
        packs: &HashMap<String, RemoteObject>,
        meter: &mut Meter,
        cache: Option<&Cache>,
        backup: bool,
    ) -> Result<(ChunkMap, usize, HashMap<String, ObjRef>)> {
        let mut referenced: HashMap<String, ObjRef> = HashMap::new();
        let z_total: u64 = idx.iter().map(|o| o.size).sum();
        // An early refusal only: a chunk entry is at most ~100 bytes of JSON, so an index of
        // more than twice the share in compressed bytes cannot fit at 40 bytes an entry.
        // Everything below that is charged entry by entry as it parses.
        if z_total > meter.limit.saturating_mul(2) {
            return Err(Error::Budget(format!(
                "{} index objects ({} compressed) exceed the index share {}",
                idx.len(),
                crate::util::human(z_total),
                crate::util::human(meter.limit)
            )));
        }
        // the pack listing is held beside the map while it loads (and by a backup after)
        let listing = packs.len() as u64 * PACK_COST;
        if !meter.fits(listing) {
            return Err(Error::Budget(format!(
                "the listing of {} packs needs about {} of the {} index share of --mem-budget, \
                 before any chunk; a larger --mem-budget holds it, up to a share of 256 MiB",
                packs.len(),
                crate::util::human(listing),
                crate::util::human(meter.limit)
            )));
        }
        meter.charge(listing)?;
        let mut map = ChunkMap::new(meter)?;
        // A backup reserves room for every entry its share can pay for, so merging a commit's
        // chunks never reallocates. A restore only loads: a chunk entry takes at least ~32
        // compressed bytes (its id is random), so z/8 bounds an honest index; past it the map
        // grows (see `ChunkMap::room_for`).
        map.reserve_share(meter, (!backup).then_some((z_total / 8) as usize + 1024));
        // fetched in parallel batches, decoded and charged one at a time in listing order; a
        // batch is decoded before the next is fetched, so at most one batch of bodies is held
        let mut z_done = 0u64;
        for r in fetch_batches(idx) {
            let group = &idx[r];
            let bodies = self.fetch_many(group, cache)?;
            for (o, b) in group.iter().zip(bodies) {
                z_done += o.size;
                if let Err(e) = self.add_index(o, &b, packs, meter, &mut map, &mut referenced) {
                    return Err(match e {
                        Error::Budget(_) => {
                            over_index_share(map.len(), z_done, z_total, meter.limit, backup)
                        }
                        e => e,
                    });
                }
                drop(b);
                crate::util::release_free_memory();
            }
        }
        // duplicates were charged as they were read; finish returns their charge
        map.finish(meter);
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
        if cfg!(feature = "broken-entry-cost") {
            // the accounting before Revision 6.4: the whole object decoded first, then charged
            let obj: IdxObj = decode(b)?;
            let mut l = IdxLoad::new(&o.key, self.kind, packs, referenced, map, meter);
            l.version(obj.v)?;
            l.kind = Some(obj.hash_kind);
            for p in &obj.packs {
                l.pack(p)?;
            }
            for c in &obj.chunks {
                l.chunk(c)?;
            }
            return Ok(());
        }
        let mut err = None;
        let r = decode_seed(
            b,
            IdxSeed {
                l: IdxLoad::new(&o.key, self.kind, packs, referenced, map, meter),
                err: &mut err,
            },
        );
        match (err, r) {
            (Some(e), _) => Err(e),
            (None, r) => r,
        }
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
    /// A deterministic id stream (xorshift64*, seed given) for the property tests.
    pub(crate) fn ids(seed: u64) -> impl FnMut() -> ChunkId {
        let mut x = seed.max(1);
        move || {
            let mut id = [0u8; 32];
            for part in id.chunks_mut(8) {
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                part.copy_from_slice(&x.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
            }
            ChunkId(id)
        }
    }

    #[test]
    fn table_model_matches_std() {
        // the hot table's sizing relies on std's growth policy; if a toolchain changes it,
        // this fails
        let mut m: HashMap<ChunkId, bool> = HashMap::new();
        let mut next = ids(1);
        for _ in 0..300_000u64 {
            m.insert(next(), false);
            let b = table_buckets(m.len());
            assert_eq!(m.capacity() as u64, table_cap(b), "map of {}", m.len());
        }
        // allocated for its capacity, the hot table never grows while it is at most half full,
        // whatever the inserts and removals (removals leave tombstones; past them, a half-full
        // table rehashes in place)
        let (b, _) = hot_table(80 * MIB);
        let room = table_cap(b) as usize / 2;
        let mut h: HashMap<ChunkId, bool> = HashMap::with_capacity(table_cap(b) as usize);
        assert_eq!(h.capacity() as u64, table_cap(b));
        let mut live: Vec<ChunkId> = Vec::new();
        let mut next = ids(7);
        let mut r = ids(11);
        for round in 0..40 {
            while live.len() < room {
                let id = next();
                h.insert(id, false);
                live.push(id);
            }
            // drop a pseudo-random half
            let mut keep = Vec::new();
            for id in live.drain(..) {
                if r().0[0] & 1 == 0 {
                    h.remove(&id);
                } else {
                    keep.push(id);
                }
            }
            live = keep;
            assert!(
                h.capacity() as u64 <= table_cap(b),
                "round {round}: the hot table grew to {}",
                h.capacity()
            );
        }
    }

    #[test]
    fn supported_sizes_per_profile() {
        // the arithmetic behind the table in crates/xfer/README.md (the table itself is
        // measured: see tests/hostile_restore.rs and tests/index_budget.rs)
        let mac = Budget::compute(640 * MIB, 16 * MIB, 4).expect("mac").i;
        let def = Budget::compute(1640 * MIB, 32 * MIB, 8).expect("default").i;
        assert_eq!(hot_table(mac), (1 << 19, 17_825_808));
        assert_eq!(hot_table(def), (1 << 19, 17_825_808));
        assert_eq!((hot_room(mac), hot_room(def)), (229_376, 229_376));
        assert_eq!(
            (backup_map_fit(mac), chunk_map_fit(mac)),
            (1_648_229, 2_090_598)
        );
        assert_eq!(
            (backup_map_fit(def), chunk_map_fit(def)),
            (4_921_753, 5_367_398)
        );
        assert_eq!(
            (
                backup_map_fit(INDEX_SHARE_MAX),
                chunk_map_fit(INDEX_SHARE_MAX)
            ),
            (6_258_687, 6_704_332)
        );
        // the advice names a budget that holds the map, and the next MiB down does not
        let need = 3_000_000 * MAP_ENTRY + slots_bytes(INDEX_SHARE_MAX);
        let m = budget_for_map(need, true).expect("a budget holds 3M chunks");
        let i = |m: u64| (256 * MIB).min(m / 8);
        assert!(i(m) >= need + hot_table(i(m)).1);
        assert!(i(m - MIB) < need + hot_table(i(m - MIB)).1);
        assert_eq!(budget_for_map(INDEX_SHARE_MAX, true), None);
    }

    fn loc(n: u32) -> Loc {
        Loc { pack: n, len: n }
    }

    #[test]
    fn chunk_map_agrees_with_a_plain_map() {
        // seed 20261121: loads with duplicates, then merges of new and already-present ids,
        // checked against a BTreeMap after every step
        let mut meter = Meter::new("test", 64 * MIB);
        let mut m = ChunkMap::new(&mut meter).expect("slots fit");
        let mut plain = std::collections::BTreeMap::new();
        let mut next = ids(20261121);
        let mut pool: Vec<ChunkId> = Vec::new();
        m.reserve_share(&meter, Some(1000));
        for i in 0..5000u32 {
            let id = if i % 7 == 0 && !pool.is_empty() {
                pool[i as usize % pool.len()]
            } else {
                let id = next();
                pool.push(id);
                id
            };
            m.push_charged(id, loc(i), &mut meter).expect("fits");
            plain.entry(id.0).or_insert(i);
        }
        m.finish(&mut meter);
        assert_eq!(m.len(), plain.len());
        for round in 0..50u32 {
            let mut add = Vec::new();
            for j in 0..(round * 37 % 400) {
                let id = if j % 3 == 0 {
                    pool[(round * 131 + j) as usize % pool.len()]
                } else {
                    let id = next();
                    pool.push(id);
                    id
                };
                add.push((id, loc(100_000 + round)));
            }
            for (id, l) in &add {
                plain.entry(id.0).or_insert(l.pack);
            }
            m.merge(add, &mut meter).expect("fits");
            assert_eq!(m.len(), plain.len(), "round {round}");
            for id in pool.iter().step_by(17) {
                let want = plain.get(&id.0).copied();
                let got = m.loc(id).map(|l| l.pack);
                // a duplicate loaded twice may keep either location; presence must agree
                assert_eq!(got.is_some(), want.is_some(), "round {round}");
                if want.unwrap_or(0) >= 100_000 {
                    assert_eq!(
                        got, want,
                        "a merged id keeps the location it was merged with"
                    );
                }
            }
            for _ in 0..20 {
                let absent = next();
                assert!(!m.contains(&absent));
            }
        }
        // the charge is 40 bytes per distinct entry plus the prefix table
        assert_eq!(
            meter.used,
            slots_bytes(64 * MIB) + plain.len() as u64 * MAP_ENTRY,
            "charged exactly"
        );
    }

    #[test]
    fn a_merge_that_does_not_fit_leaves_the_map_unchanged() {
        let lim = 100 * MAP_ENTRY + slots_bytes(100 * MAP_ENTRY);
        let sb = slots_bytes(lim);
        let mut meter = Meter::new("test", lim);
        let mut m = ChunkMap::new(&mut meter).expect("slots fit");
        let mut next = ids(3);
        let first: Vec<_> = (0..90).map(|i| (next(), loc(i))).collect();
        m.merge(first.clone(), &mut meter).expect("90 fit");
        let more: Vec<_> = (0..20).map(|i| (next(), loc(i))).collect();
        assert!(matches!(
            m.merge(more.clone(), &mut meter),
            Err(Error::Budget(_))
        ));
        assert_eq!(m.len(), 90);
        assert!(more.iter().all(|(id, _)| !m.contains(id)));
        assert!(first.iter().all(|(id, _)| m.contains(id)));
        assert_eq!(meter.used, sb + 90 * MAP_ENTRY);
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
