//! The preserve obligation: the files of a git directory that a removal must back up first.
//!
//! A git directory holds two kinds of files. Object files are the object store's copies of what
//! the commits contain: loose objects, packs with their indexes, reverse indexes, bitmaps and
//! cruft mtimes, multi-pack-indexes and commit-graphs. Once every commit is shown to be on a
//! network remote they hold nothing the remote lacks, and each one is checked here to be what
//! its name says (a loose object's zlib header, a pack's and an index's magic and their pairing,
//! the magic of the others). Every other file is the git directory's own: config, hooks, refs,
//! logs, the index, messages, rerere's records, `.keep` and `.promisor` markers, `objects/info`
//! lists, and anything a person put anywhere in it. A plan never removes a git node unless a
//! verified backup of all of these exists (`docs/SCAN-FORMAT.md`, "The preserve obligation").
//!
//! [`sort`] is the one classifier: the scan's [`survey`] and [`git_admin_files`], which a later
//! apply phase uses to make the backup, both go through it, so they always agree.

use crate::gitshape::{fanout, loose_object, named, oid};
use crate::model::{AdminFile, Class, GitAdminSurvey, Node};
use std::collections::HashSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Most entries walked in one git directory; more cannot be enumerated and is an error.
pub const MAX_ENTRIES: u64 = 2_000_000;
/// Most loose objects whose header is checked in one git directory; more is an error.
pub const MAX_LOOSE_CHECKED: u64 = 50_000;
/// Bytes read from a loose object: enough for zlib's header, any block header and the object
/// header `<type> <size>\0`.
const LOOSE_READ: usize = 2048;
/// Examples kept in a [`GitAdminSurvey`].
const EXAMPLES: usize = 5;
/// Largest files kept in a [`GitAdminSurvey`].
const LARGEST: usize = 3;

/// The hooks `git init` installs from its stock template (git 2.56).
const STOCK_HOOKS: [&str; 14] = [
    "applypatch-msg.sample",
    "commit-msg.sample",
    "fsmonitor-watchman.sample",
    "post-update.sample",
    "pre-applypatch.sample",
    "pre-commit.sample",
    "pre-merge-commit.sample",
    "pre-push.sample",
    "pre-rebase.sample",
    "pre-receive.sample",
    "prepare-commit-msg.sample",
    "push-to-checkout.sample",
    "sendemail-validate.sample",
    "update.sample",
];

/// One git directory's files, sorted.
#[derive(Debug, Default)]
pub struct Sorted {
    /// Its own files (everything that is not an object file): path relative to the git directory
    /// with `/` separators, and allocated bytes. Sorted by path.
    pub own: Vec<(String, u64)>,
    /// Files named like object files that are not what their name says. Each is also in `own`:
    /// it is not a git object, so it is the git directory's own content.
    pub not_objects: Vec<String>,
}

/// What [`survey`] found: the counts for the plan, and the files named like objects that are not.
#[derive(Debug, Default)]
pub struct Survey {
    pub admin: GitAdminSurvey,
    pub not_objects: Vec<String>,
}

/// The git directory a node's removal deletes: `<path>/.git` for a repo or worktree whose `.git`
/// is a real directory, the directory itself for a bare repo.
pub fn git_dir_of(node: &Node) -> io::Result<PathBuf> {
    let path = Path::new(&node.path);
    match node.class {
        Class::BareRepo => Ok(path.to_path_buf()),
        Class::Repo | Class::Worktree => {
            let gd = path.join(".git");
            if fs::symlink_metadata(&gd)?.is_dir() {
                Ok(gd)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the .git entry is not a directory, so the git directory lies elsewhere",
                ))
            }
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a repo, worktree or bare repo",
        )),
    }
}

/// Every file in `node`'s git directory that is not an object file of its object store, as
/// paths relative to the git directory with `/` separators, sorted. These are what a removal
/// must back up, and verify, before it removes anything. An error (an unreadable directory, a
/// name that is not UTF-8, too many entries or loose objects) means the list is not complete:
/// nothing may be removed then.
pub fn git_admin_files(node: &Node) -> io::Result<Vec<String>> {
    let gd = git_dir_of(node)?;
    Ok(sort(&gd)?.own.into_iter().map(|(p, _)| p).collect())
}

/// Count `git_dir`'s own files for the plan (see [`sort`]).
pub fn survey(git_dir: &Path) -> io::Result<Survey> {
    let sorted = sort(git_dir)?;
    let mut admin = GitAdminSurvey {
        files: sorted.own.len() as u64,
        bytes: sorted.own.iter().map(|(_, b)| *b).sum(),
        ..GitAdminSurvey::default()
    };
    admin.examples = sorted
        .own
        .iter()
        .filter(|(p, _)| !everyday(p))
        .take(EXAMPLES)
        .map(|(p, _)| p.clone())
        .collect();
    let mut by_size: Vec<&(String, u64)> = sorted.own.iter().collect();
    by_size.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    admin.largest = by_size
        .into_iter()
        .take(LARGEST)
        .map(|(p, b)| AdminFile {
            path: p.clone(),
            bytes: *b,
        })
        .collect();
    Ok(Survey {
        admin,
        not_objects: sorted.not_objects,
    })
}

/// A name every clone has (or git writes in ordinary use): not worth naming as an example.
/// Examples go by name only; the backup covers every file whatever its name or content.
fn everyday(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.as_slice() {
        [top] => matches!(
            *top,
            "HEAD"
                | "config"
                | "description"
                | "index"
                | "packed-refs"
                | "FETCH_HEAD"
                | "ORIG_HEAD"
                | "shallow"
        ),
        ["info", "exclude" | "refs"] | ["objects", "info", "packs"] | ["logs", "HEAD"] => true,
        ["hooks", name] => STOCK_HOOKS.contains(name),
        ["refs", rest @ ..] | ["logs", "refs", rest @ ..] => {
            !rest.is_empty() && rest.iter().all(|p| !p.starts_with('.'))
        }
        _ => false,
    }
}

/// What a file's name says it is, when it names an object file.
enum Expect {
    /// `objects/<xx>/<rest of the id>`.
    Loose,
    /// A file that starts with these bytes.
    Magic(&'static [u8]),
    /// `objects/pack/pack-<id>.<ext>`: its stem (`pack-<id>`) and extension.
    Pack(String, &'static str),
}

/// The pack-side files that are object files (`.keep` and `.promisor` are markers a person or a
/// partial clone set: they are the git directory's own).
const PACK_OBJECT_EXTS: [&str; 5] = ["pack", "idx", "rev", "bitmap", "mtimes"];

fn expect(parts: &[&str]) -> Option<Expect> {
    match parts {
        ["objects", d, f] if fanout(d) && loose_object(f) => Some(Expect::Loose),
        ["objects", "pack", "multi-pack-index"] => Some(Expect::Magic(b"MIDX")),
        ["objects", "pack", f] if named(f, "multi-pack-index-", &["bitmap"]) => {
            Some(Expect::Magic(b"BITM"))
        }
        ["objects", "pack", f] if named(f, "multi-pack-index-", &["rev"]) => {
            Some(Expect::Magic(b"RIDX"))
        }
        ["objects", "pack", f] if named(f, "pack-", &PACK_OBJECT_EXTS) => {
            let (stem, ext) = f.split_once('.')?;
            let ext: &'static str = PACK_OBJECT_EXTS.iter().copied().find(|e| *e == ext)?;
            Some(Expect::Pack(stem.to_string(), ext))
        }
        ["objects", "pack", "multi-pack-index.d", f] => {
            let (stem, ext) = f.split_once('.')?;
            let id = stem.strip_prefix("multi-pack-index-")?;
            if !oid(id) {
                return None;
            }
            match ext {
                "midx" => Some(Expect::Magic(b"MIDX")),
                "bitmap" => Some(Expect::Magic(b"BITM")),
                "rev" => Some(Expect::Magic(b"RIDX")),
                _ => None,
            }
        }
        ["objects", "info", "commit-graph"] => Some(Expect::Magic(b"CGPH")),
        ["objects", "info", "commit-graphs", f] if named(f, "graph-", &["graph"]) => {
            Some(Expect::Magic(b"CGPH"))
        }
        _ => None,
    }
}

/// Allocated bytes of an entry (blocks on Unix, the length elsewhere).
#[cfg(unix)]
fn allocated(md: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated(md: &fs::Metadata) -> u64 {
    md.len()
}

/// Up to `n` bytes from the start of `path`.
fn head(path: &Path, n: usize) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(n);
    fs::File::open(path)?.take(n as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

fn starts_with(path: &Path, magic: &[u8]) -> io::Result<bool> {
    Ok(head(path, magic.len())?.as_slice() == magic)
}

/// A pack: `PACK` and version 2 or 3.
fn pack_ok(path: &Path) -> io::Result<bool> {
    let h = head(path, 8)?;
    Ok(h.len() == 8 && h[..4] == *b"PACK" && matches!(&h[4..8], [0, 0, 0, 2] | [0, 0, 0, 3]))
}

/// A pack index: version 2 (`\377tOc`, then 2), or version 1 (no magic: 256 big-endian counts
/// that never decrease, then 24 bytes per object and two 20-byte checksums).
fn idx_ok(path: &Path, len: u64) -> io::Result<bool> {
    let h = head(path, 1024)?;
    if h.len() >= 8 && h[..4] == [0xff, b't', b'O', b'c'] {
        return Ok(matches!(&h[4..8], [0, 0, 0, 2] | [0, 0, 0, 3]));
    }
    if h.len() < 1024 {
        return Ok(false);
    }
    let mut prev = 0u32;
    for k in h.as_chunks::<4>().0 {
        let n = u32::from_be_bytes(*k);
        if n < prev {
            return Ok(false);
        }
        prev = n;
    }
    Ok(len == 1024 + 24 * u64::from(prev) + 40)
}

/// A loose object: a zlib stream whose inflated start is `<type> <size>\0`.
fn loose_ok(path: &Path) -> io::Result<bool> {
    let data = head(path, LOOSE_READ)?;
    Ok(inflate::head(&data).is_some_and(|out| header_ok(&out)))
}

/// `blob|tree|commit|tag`, a space, 1 to 20 decimal digits and a NUL, nothing else.
fn header_ok(out: &[u8]) -> bool {
    let Some((&0, head)) = out.split_last() else {
        return false;
    };
    let Some(sp) = head.iter().position(|&b| b == b' ') else {
        return false;
    };
    let (kind, size) = (&head[..sp], &head[sp + 1..]);
    matches!(kind, b"blob" | b"tree" | b"commit" | b"tag")
        && (1..=20).contains(&size.len())
        && size.iter().all(u8::is_ascii_digit)
}

/// Sort every entry of `git_dir` into its own files and object files, checking each file named
/// like an object file by content (see the module documentation). Symlinks are never followed;
/// a symlink or any other non-directory entry is a file of its own. Bounded: more than
/// [`MAX_ENTRIES`] entries or [`MAX_LOOSE_CHECKED`] loose objects is an error, as is any entry
/// that cannot be listed, read or named in UTF-8.
pub fn sort(git_dir: &Path) -> io::Result<Sorted> {
    let mut out = Sorted::default();
    let mut seen = 0u64;
    let mut loose = 0u64;
    let mut stack: Vec<(PathBuf, String)> = vec![(git_dir.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let mut entries: Vec<(String, fs::Metadata)> = Vec::new();
        for e in fs::read_dir(&dir)? {
            let e = e?;
            seen += 1;
            if seen > MAX_ENTRIES {
                return Err(io::Error::other(format!(
                    "more than {MAX_ENTRIES} entries in the git directory"
                )));
            }
            let name = e.file_name().into_string().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "an entry in the git directory has a name that is not UTF-8",
                )
            })?;
            entries.push((name, e.metadata()?));
        }
        // Packs whose pack file and index are both what they say: only their files are objects.
        let mut good_packs: HashSet<String> = HashSet::new();
        if rel == "objects/pack" {
            let (mut packs, mut idxs) = (HashSet::new(), HashSet::new());
            for (name, md) in &entries {
                if !md.is_file() {
                    continue;
                }
                let path = dir.join(name);
                if let Some(stem) = name.strip_suffix(".pack") {
                    if named(name, "pack-", &["pack"]) && pack_ok(&path)? {
                        packs.insert(stem.to_string());
                    }
                } else if let Some(stem) = name.strip_suffix(".idx") {
                    if named(name, "pack-", &["idx"]) && idx_ok(&path, md.len())? {
                        idxs.insert(stem.to_string());
                    }
                }
            }
            good_packs = packs.intersection(&idxs).cloned().collect();
        }
        for (name, md) in entries {
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if md.is_dir() {
                stack.push((dir.join(&name), child));
                continue;
            }
            let parts: Vec<&str> = child.split('/').collect();
            let object = match expect(&parts) {
                None => false,
                Some(_) if !md.is_file() => {
                    out.not_objects.push(child.clone());
                    false
                }
                Some(want) => {
                    let path = dir.join(&name);
                    let ok = match want {
                        Expect::Loose => {
                            loose += 1;
                            if loose > MAX_LOOSE_CHECKED {
                                return Err(io::Error::other(format!(
                                    "more than {MAX_LOOSE_CHECKED} loose objects to check"
                                )));
                            }
                            loose_ok(&path)?
                        }
                        Expect::Magic(m) => starts_with(&path, m)?,
                        Expect::Pack(stem, ext) => {
                            good_packs.contains(&stem)
                                && match ext {
                                    "rev" => starts_with(&path, b"RIDX")?,
                                    "bitmap" => starts_with(&path, b"BITM")?,
                                    "mtimes" => starts_with(&path, b"MTME")?,
                                    _ => true,
                                }
                        }
                    };
                    if !ok {
                        out.not_objects.push(child.clone());
                    }
                    ok
                }
            };
            if !object {
                out.own.push((child, allocated(&md)));
            }
        }
    }
    out.own.sort();
    out.not_objects.sort();
    Ok(out)
}

/// A bounded inflater: just enough of zlib (RFC 1950) and DEFLATE (RFC 1951) to read the first
/// bytes of a loose object, stopping at the NUL that ends its header or after [`MAX_OUT`]
/// bytes. It never needs the end of the stream or its checksum, and it never panics: any
/// malformed input is `None`.
mod inflate {
    const LBASE: [u16; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    const LEXT: [u8; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    const DBASE: [u16; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const DEXT: [u8; 30] = [
        0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
        13, 13,
    ];
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];

    /// Most output bytes produced: a loose object's header is far shorter.
    pub(super) const MAX_OUT: usize = 64;

    struct Bits<'a> {
        data: &'a [u8],
        pos: usize,
        bit: u32,
    }

    impl Bits<'_> {
        /// `n` bits, least significant first.
        fn get(&mut self, n: u32) -> Option<u32> {
            let mut v = 0u32;
            for i in 0..n {
                let byte = *self.data.get(self.pos)?;
                v |= u32::from((byte >> self.bit) & 1) << i;
                self.bit += 1;
                if self.bit == 8 {
                    self.bit = 0;
                    self.pos += 1;
                }
            }
            Some(v)
        }

        fn align(&mut self) {
            if self.bit != 0 {
                self.bit = 0;
                self.pos += 1;
            }
        }
    }

    /// A canonical Huffman code: how many codes of each length, and the symbols in code order.
    struct Huff {
        count: [u16; 16],
        symbol: Vec<u16>,
    }

    /// Every length is at most 15 (the callers only pass such lengths).
    fn build(lengths: &[u8]) -> Huff {
        let mut count = [0u16; 16];
        for &l in lengths {
            count[usize::from(l & 15)] += 1;
        }
        count[0] = 0;
        // Where the codes of each length start in `symbol`: after all shorter ones.
        let mut offs = [0u16; 16];
        let mut start = 0u16;
        for (o, c) in offs.iter_mut().zip(count.iter()).skip(1) {
            *o = start;
            start += c;
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                let o = &mut offs[usize::from(l & 15)];
                if let Some(slot) = symbol.get_mut(usize::from(*o)) {
                    *slot = s as u16;
                }
                *o += 1;
            }
        }
        Huff { count, symbol }
    }

    fn decode(bits: &mut Bits, h: &Huff) -> Option<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for &n in &h.count[1..] {
            code |= bits.get(1)? as i32;
            let c = i32::from(n);
            if code - c < first {
                let k = usize::try_from(index + (code - first)).ok()?;
                return h.symbol.get(k).copied();
            }
            index += c;
            first += c;
            first <<= 1;
            code <<= 1;
        }
        None
    }

    fn done(out: &[u8]) -> bool {
        out.len() >= MAX_OUT || out.last() == Some(&0)
    }

    /// One block's symbols into `out`: `Some(true)` when the header is complete, `Some(false)` at
    /// the end of the block.
    fn codes(bits: &mut Bits, out: &mut Vec<u8>, lit: &Huff, dist: &Huff) -> Option<bool> {
        loop {
            let sym = decode(bits, lit)?;
            if let Ok(byte) = u8::try_from(sym) {
                out.push(byte);
                if done(out) {
                    return Some(true);
                }
            } else if sym == 256 {
                return Some(false);
            } else {
                let s = usize::from(sym - 257);
                let extra = bits.get(u32::from(*LEXT.get(s)?))? as usize;
                let length = usize::from(*LBASE.get(s)?) + extra;
                let ds = usize::from(decode(bits, dist)?);
                let extra = bits.get(u32::from(*DEXT.get(ds)?))? as usize;
                let d = usize::from(*DBASE.get(ds)?) + extra;
                if d > out.len() {
                    return None;
                }
                for _ in 0..length {
                    let b = out[out.len() - d];
                    out.push(b);
                    if done(out) {
                        return Some(true);
                    }
                }
            }
        }
    }

    fn fixed() -> (Huff, Huff) {
        let mut lit = [8u8; 288];
        lit[144..256].fill(9);
        lit[256..280].fill(7);
        (build(&lit), build(&[5u8; 30]))
    }

    fn dynamic(bits: &mut Bits) -> Option<(Huff, Huff)> {
        let nlen = bits.get(5)? as usize + 257;
        let ndist = bits.get(5)? as usize + 1;
        let ncode = bits.get(4)? as usize + 4;
        if nlen > 286 || ndist > 30 {
            return None;
        }
        let mut cl = [0u8; 19];
        for &k in ORDER.iter().take(ncode) {
            cl[k] = bits.get(3)? as u8;
        }
        let clh = build(&cl);
        let mut lengths: Vec<u8> = Vec::with_capacity(nlen + ndist);
        while lengths.len() < nlen + ndist {
            let (value, times) = match decode(bits, &clh)? {
                s @ 0..=15 => (s as u8, 1),
                16 => (*lengths.last()?, 3 + bits.get(2)? as usize),
                17 => (0, 3 + bits.get(3)? as usize),
                _ => (0, 11 + bits.get(7)? as usize),
            };
            lengths.resize(lengths.len() + times, value);
        }
        if lengths.len() > nlen + ndist {
            return None;
        }
        Some((build(&lengths[..nlen]), build(&lengths[nlen..])))
    }

    /// The first bytes of the zlib stream `data`: up to and including the first NUL, or
    /// [`MAX_OUT`] bytes, or all of a shorter stream. `None` for anything malformed or cut short.
    pub(super) fn head(data: &[u8]) -> Option<Vec<u8>> {
        let (cmf, flg) = (*data.first()?, *data.get(1)?);
        let check = (u32::from(cmf) * 256 + u32::from(flg)) % 31;
        if cmf & 0x0f != 8 || cmf >> 4 > 7 || check != 0 || flg & 0x20 != 0 {
            return None;
        }
        let mut bits = Bits {
            data,
            pos: 2,
            bit: 0,
        };
        let mut out: Vec<u8> = Vec::with_capacity(MAX_OUT);
        loop {
            let last = bits.get(1)?;
            match bits.get(2)? {
                0 => {
                    bits.align();
                    let p = bits.pos;
                    let n = u16::from_le_bytes([*data.get(p)?, *data.get(p + 1)?]);
                    let nn = u16::from_le_bytes([*data.get(p + 2)?, *data.get(p + 3)?]);
                    if n != !nn {
                        return None;
                    }
                    bits.pos += 4;
                    for _ in 0..n {
                        out.push(*data.get(bits.pos)?);
                        bits.pos += 1;
                        if done(&out) {
                            return Some(out);
                        }
                    }
                }
                1 => {
                    let (lit, dist) = fixed();
                    if codes(&mut bits, &mut out, &lit, &dist)? {
                        return Some(out);
                    }
                }
                2 => {
                    let (lit, dist) = dynamic(&mut bits)?;
                    if codes(&mut bits, &mut out, &lit, &dist)? {
                        return Some(out);
                    }
                }
                _ => return None,
            }
            if last == 1 {
                return Some(out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Made with Python's zlib: `blob 5\0hello` at level 0 (a stored block), `commit 12\0tree
    // abcdefgh` at level 1 (fixed Huffman), and the first 64 bytes of 60 lines of text as a blob
    // at level 9 (dynamic Huffman).
    const STORED: [u8; 23] = [
        0x78, 0x01, 0x01, 0x0c, 0x00, 0xf3, 0xff, 0x62, 0x6c, 0x6f, 0x62, 0x20, 0x35, 0x00, 0x68,
        0x65, 0x6c, 0x6c, 0x6f, 0x19, 0xaa, 0x04, 0x09,
    ];
    const FIXED: [u8; 31] = [
        0x78, 0x01, 0x4b, 0xce, 0xcf, 0xcd, 0xcd, 0x2c, 0x51, 0x30, 0x34, 0x62, 0x28, 0x29, 0x4a,
        0x4d, 0x55, 0x48, 0x4c, 0x4a, 0x4e, 0x49, 0x4d, 0x4b, 0xcf, 0x00, 0x00, 0x5e, 0xa9, 0x08,
        0x01,
    ];
    const DYNAMIC: [u8; 64] = [
        0x78, 0xda, 0x6d, 0xd4, 0x3b, 0x6e, 0x02, 0x41, 0x10, 0x40, 0x41, 0xc7, 0x9c, 0x62, 0x8e,
        0x40, 0xff, 0x30, 0xbe, 0x0e, 0xd2, 0x22, 0x59, 0x5a, 0xb3, 0x81, 0x09, 0x38, 0x3e, 0x92,
        0x43, 0xab, 0xd2, 0x17, 0x75, 0xa9, 0x7b, 0xe6, 0xb6, 0x1f, 0xb7, 0x15, 0x39, 0xe7, 0x8f,
        0xfd, 0xfb, 0xb1, 0xad, 0xf3, 0x3a, 0xee, 0xeb, 0xf7, 0xf8, 0xd9, 0xd6, 0x73, 0x7b, 0x3d,
        0x4f, 0x7f, 0x2d, 0xd0,
    ];

    #[test]
    fn the_inflater_reads_the_header_from_every_kind_of_block() {
        assert_eq!(inflate::head(&STORED).as_deref(), Some(&b"blob 5\0"[..]));
        assert_eq!(inflate::head(&FIXED).as_deref(), Some(&b"commit 12\0"[..]));
        assert_eq!(
            inflate::head(&DYNAMIC).as_deref(),
            Some(&b"blob 1250\0"[..])
        );
        for good in [&STORED[..], &FIXED[..], &DYNAMIC[..]] {
            assert!(inflate::head(good).is_some_and(|h| header_ok(&h)));
        }
    }

    #[test]
    fn anything_else_is_not_a_loose_object_and_never_panics() {
        for bad in [
            &b""[..],
            b"x",
            b"not a zlib stream at all",
            &STORED[..10],
            &FIXED[..8],
            &DYNAMIC[..20],
        ] {
            assert!(
                !inflate::head(bad).is_some_and(|h| header_ok(&h)),
                "{bad:?}"
            );
        }
        // Every truncation and every single-byte change of a valid stream is handled.
        for v in [&STORED[..], &FIXED[..], &DYNAMIC[..]] {
            for cut in 0..v.len() {
                let _ = inflate::head(&v[..cut]);
            }
            for i in 0..v.len() {
                for x in [0x01u8, 0x80, 0xff] {
                    let mut w = v.to_vec();
                    w[i] ^= x;
                    let _ = inflate::head(&w);
                }
            }
        }
    }

    #[test]
    fn headers_must_name_an_object_type_and_a_size() {
        assert!(header_ok(b"blob 0\0"));
        assert!(header_ok(b"tag 123\0"));
        for bad in [
            &b"blob 5"[..],
            b"blob \0",
            b"blobs 5\0",
            b"blob x\0",
            b"blob 123456789012345678901\0",
            b"\0",
        ] {
            assert!(!header_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn names_say_which_files_are_object_files() {
        let parts = |s: &'static str| s.split('/').collect::<Vec<_>>();
        let id = "0123456789abcdef0123456789abcdef01234567";
        let object = |s: String| {
            let p: Vec<&str> = s.split('/').collect();
            expect(&p).is_some()
        };
        assert!(object(format!("objects/ab/{}", &id[2..])));
        assert!(object(format!("objects/pack/pack-{id}.pack")));
        assert!(object(format!("objects/pack/pack-{id}.mtimes")));
        assert!(object(format!("objects/pack/multi-pack-index-{id}.bitmap")));
        assert!(object(format!(
            "objects/pack/multi-pack-index.d/multi-pack-index-{id}.midx"
        )));
        assert!(object(format!(
            "objects/info/commit-graphs/graph-{id}.graph"
        )));
        assert!(expect(&parts("objects/pack/multi-pack-index")).is_some());
        assert!(expect(&parts("objects/info/commit-graph")).is_some());
        // Markers, lists, chains and anything else are the git directory's own.
        assert!(!object(format!("objects/pack/pack-{id}.keep")));
        assert!(!object(format!("objects/pack/pack-{id}.promisor")));
        for own in [
            "objects/info/packs",
            "objects/info/alternates",
            "objects/info/commit-graphs/commit-graph-chain",
            "objects/pack/multi-pack-index.d/multi-pack-index-chain",
            "objects/notes.txt",
            "objects/ab/README",
            "HEAD",
            "hooks/pre-commit.sample",
        ] {
            assert!(expect(&parts(own)).is_none(), "{own}");
        }
    }

    #[test]
    fn examples_skip_names_every_clone_has() {
        for every in [
            "HEAD",
            "config",
            "index",
            "info/exclude",
            "hooks/pre-commit.sample",
            "refs/heads/main",
            "logs/refs/remotes/origin/HEAD",
            "logs/HEAD",
        ] {
            assert!(everyday(every), "{every}");
        }
        for unusual in [
            "COMMIT_EDITMSG",
            "hooks/custom.sample",
            "hooks/pre-commit",
            "logs/refs/.notes",
            "rr-cache/x/postimage",
            "objects/pack/pack-x.keep",
        ] {
            assert!(!everyday(unusual), "{unusual}");
        }
    }

    #[test]
    fn an_index_is_checked_by_its_layout() {
        let dir = tempfile::tempdir().unwrap();
        let v2 = dir.path().join("v2.idx");
        fs::write(&v2, [0xff, b't', b'O', b'c', 0, 0, 0, 2, 9, 9]).unwrap();
        assert!(idx_ok(&v2, 10).unwrap());
        // Version 1 with no objects: 256 zero counts and two checksums.
        let v1 = dir.path().join("v1.idx");
        fs::write(&v1, vec![0u8; 1024 + 40]).unwrap();
        assert!(idx_ok(&v1, 1064).unwrap());
        assert!(!idx_ok(&v1, 1065).unwrap());
        let text = dir.path().join("text.idx");
        fs::write(&text, "not an index\n").unwrap();
        assert!(!idx_ok(&text, 13).unwrap());
        let pack = dir.path().join("p.pack");
        fs::write(&pack, b"PACK\0\0\0\x02rest").unwrap();
        assert!(pack_ok(&pack).unwrap());
        fs::write(&pack, b"PACK\0\0\0\x09rest").unwrap();
        assert!(!pack_ok(&pack).unwrap());
    }
}
