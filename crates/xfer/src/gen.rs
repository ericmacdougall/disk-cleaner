//! Deterministic test and benchmark data. Everything here is generated from a seed, so a workload
//! is reproducible byte for byte; nothing reads the user's own files.

use crate::error::Result;
use std::fs;
use std::io::Write;
use std::path::Path;

/// SplitMix64: small, fast and good enough for test data. Not for cryptography.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }
    /// Incompressible bytes.
    pub fn fill(&mut self, buf: &mut [u8]) {
        for c in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            c.copy_from_slice(&v[..c.len()]);
        }
    }
    /// Text-like bytes that zstd compresses about 3-4x.
    pub fn fill_text(&mut self, buf: &mut [u8]) {
        const WORDS: &[&[u8]] = &[
            b"the ",
            b"backup ",
            b"engine ",
            b"object ",
            b"storage ",
            b"chunk ",
            b"pack ",
            b"index ",
            b"verify ",
            b"restore ",
            b"folder ",
            b"file ",
            b"large ",
            b"small ",
            b"bytes ",
            b"of ",
            b"and ",
            b"to ",
            b"a ",
            b"in ",
            b"2026-11-20 ",
            b"INFO ",
            b"WARN ",
            b"{\"k\": ",
            b"},\n",
            b"\n",
            b"0x",
            b"ff ",
            b"error ",
            b"ok ",
        ];
        let mut i = 0;
        while i < buf.len() {
            let mut w: &[u8] = WORDS[self.below(WORDS.len() as u64) as usize];
            if self.below(16) == 0 {
                // a little noise so it is not trivially compressible
                let n = self.next_u64().to_le_bytes();
                let len = (buf.len() - i).min(4);
                buf[i..i + len].copy_from_slice(&n[..len]);
                i += len;
                continue;
            }
            if w.len() > buf.len() - i {
                w = &w[..buf.len() - i];
            }
            buf[i..i + w.len()].copy_from_slice(w);
            i += w.len();
        }
    }
}

/// A generated workload. Sizes are totals; see the spec's workload table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Workload {
    /// Duplicate-heavy mixed tree in the proportions of the spec's W1 table: archives, a binary
    /// image, camera raw, small code files, about 42% exact duplicate bytes, hard links and
    /// symlinks.
    W1,
    /// One large incompressible file.
    W2,
    /// Many small files.
    W3,
    /// The small mixed tree used on the Mac.
    Mac,
}

impl Workload {
    pub fn parse(s: &str) -> Option<Workload> {
        match s {
            "w1" => Some(Workload::W1),
            "w2" => Some(Workload::W2),
            "w3" => Some(Workload::W3),
            "mac" => Some(Workload::Mac),
            _ => None,
        }
    }
}

fn write_file(p: &Path, data: &[u8]) -> Result<()> {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d)?;
    }
    let mut f = fs::File::create(p)?;
    f.write_all(data)?;
    Ok(())
}

/// How a generated large file's bytes look to a compressor.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fill {
    /// Incompressible (archives, media, camera raw).
    Random,
    /// Text, about 3-4x under zstd.
    Text,
    /// Binary that compresses a little: 64 KiB blocks, half incompressible and half text.
    Mixed,
}

fn write_stream(p: &Path, total: u64, rng: &mut Rng, fill: Fill) -> Result<()> {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d)?;
    }
    let mut f = std::io::BufWriter::with_capacity(1 << 20, fs::File::create(p)?);
    let mut buf = vec![0u8; 64 << 10];
    let mut left = total;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
        let text = match fill {
            Fill::Random => false,
            Fill::Text => true,
            Fill::Mixed => rng.below(2) == 0,
        };
        if text {
            rng.fill_text(&mut buf[..n]);
        } else {
            rng.fill(&mut buf[..n]);
        }
        f.write_all(&buf[..n])?;
        left -= n as u64;
    }
    f.flush()?;
    Ok(())
}

fn copy_into(from: &Path, to: &Path) -> Result<()> {
    if let Some(d) = to.parent() {
        fs::create_dir_all(d)?;
    }
    fs::copy(from, to)?;
    Ok(())
}

/// Generate a workload under `dir` scaled to roughly `total` bytes. Returns the file count.
pub fn generate(dir: &Path, w: Workload, total: u64, seed: u64) -> Result<u64> {
    fs::create_dir_all(dir)?;
    let mut rng = Rng::new(seed);
    let mut files = 0u64;
    match w {
        Workload::W2 => {
            write_stream(&dir.join("big.bin"), total, &mut rng, Fill::Random)?;
            files = 1;
        }
        Workload::W3 => {
            // 512 B to 7.5 KiB files (mean about 4 KiB, so 800 MiB is about 200k files, the
            // spec's W3), 60% text, spread over 256 directories
            let mut written = 0u64;
            let mut buf = vec![0u8; 8 << 10];
            while written < total {
                let n = (512 + rng.below(7 << 10)) as usize;
                let text = rng.below(10) < 6;
                if text {
                    rng.fill_text(&mut buf[..n]);
                } else {
                    rng.fill(&mut buf[..n]);
                }
                let p = dir.join(format!(
                    "d{:03}/f{:07}.{}",
                    files % 256,
                    files,
                    if text { "txt" } else { "bin" }
                ));
                write_file(&p, &buf[..n])?;
                written += n as u64;
                files += 1;
            }
        }
        Workload::W1 | Workload::Mac => {
            // The spec's W1 table, modelled on the measured 48 GiB folder. By bytes, duplicates
            // included: about 56% zip (incompressible), 29% bin (compresses a little), 6% camera
            // raw (incompressible) and 6% text/code (small files), plus 0.5% logs. About 42% of
            // all bytes are exact whole-file duplicates of other files in the tree:
            //   zip:  8 archives = 34%, copies of the first 5 = 21%
            //   bin:  one image  = 14.5%, one exact copy     = 14.5%
            //   raw:  8 photos   = 3%,  a copied folder      = 3%
            //   code: small files = 3%, a copied project     = 3%
            let pct = |p: u64| total * p / 1000;
            for i in 0..8 {
                let p = dir.join(format!("archives/a{i}.zip"));
                write_stream(&p, pct(340) / 8, &mut rng, Fill::Random)?;
                files += 1;
                if i < 5 {
                    copy_into(&p, &dir.join(format!("downloads/a{i} (1).zip")))?;
                    files += 1;
                }
            }
            write_stream(&dir.join("vm/disk-a.img"), pct(145), &mut rng, Fill::Mixed)?;
            copy_into(
                &dir.join("vm/disk-a.img"),
                &dir.join("copies/old/disk-a.img"),
            )?;
            files += 2;
            for i in 0..8 {
                let p = dir.join(format!("photos/raw/IMG_{i:04}.CR2"));
                write_stream(&p, pct(30) / 8, &mut rng, Fill::Random)?;
                copy_into(&p, &dir.join(format!("photos-copy/raw/IMG_{i:04}.CR2")))?;
                files += 2;
            }
            for i in 0..3 {
                let p = dir.join(format!("logs/log{i}.txt"));
                write_stream(&p, pct(5) / 3, &mut rng, Fill::Text)?;
                files += 1;
            }
            let mut written = 0u64;
            let mut buf = vec![0u8; 8 << 10];
            // 512 B to 7.5 KiB (mean about 4 KiB): about 30k files at 4 GiB, doubled by the copy
            let mut small = Vec::new();
            while written < pct(30) {
                let n = (512 + rng.below(7 << 10)) as usize;
                rng.fill_text(&mut buf[..n]);
                let rel = format!("m{:03}/f{:06}.rs", small.len() % 97, small.len());
                write_file(&dir.join("src").join(&rel), &buf[..n])?;
                small.push(rel);
                written += n as u64;
                files += 1;
            }
            // a copied project folder: exact duplicates of every small file
            for rel in &small {
                copy_into(
                    &dir.join("src").join(rel),
                    &dir.join("src-backup").join(rel),
                )?;
                files += 1;
            }
            #[cfg(unix)]
            {
                fs::create_dir_all(dir.join("links"))?;
                fs::hard_link(dir.join("logs/log0.txt"), dir.join("links/log0-hard.txt"))?;
                std::os::unix::fs::symlink("../logs/log1.txt", dir.join("links/log1-sym.txt"))?;
                std::os::unix::fs::symlink("does-not-exist", dir.join("links/dangling"))?;
                files += 3;
                // about 70 hard links in all (the spec's W1): link the first small files of m002
                let m = dir.join("src/m002");
                if m.is_dir() {
                    let mut names: Vec<_> = fs::read_dir(&m)?
                        .map(|e| e.map(|e| e.file_name()))
                        .collect::<std::io::Result<_>>()?;
                    names.sort();
                    fs::create_dir_all(dir.join("links/hard"))?;
                    for n in names.iter().take(69) {
                        fs::hard_link(m.join(n), dir.join("links/hard").join(n))?;
                        files += 1;
                    }
                }
            }
        }
    }
    Ok(files)
}

/// A small change for the second-run workload (W4): append to one text file, rewrite 64 KiB in
/// the middle of one large binary file, add 200 new small files and delete one small file.
pub fn mutate(dir: &Path, seed: u64) -> Result<()> {
    let mut rng = Rng::new(seed ^ 0xABCD);
    let mut buf = vec![0u8; 64 << 10];
    let log = dir.join("logs/log2.txt");
    if log.is_file() {
        rng.fill_text(&mut buf);
        fs::OpenOptions::new()
            .append(true)
            .open(&log)?
            .write_all(&buf)?;
    }
    let img = dir.join("vm/disk-a.img");
    if img.is_file() {
        use std::io::{Seek, SeekFrom};
        let len = fs::metadata(&img)?.len();
        let mut f = fs::OpenOptions::new().write(true).open(&img)?;
        f.seek(SeekFrom::Start(len / 2))?;
        rng.fill(&mut buf);
        f.write_all(&buf)?;
    }
    // 200 new small files (the spec's W4)
    for i in 0..200 {
        let n = 512 + rng.below(7 << 10) as usize;
        rng.fill_text(&mut buf[..n]);
        write_file(&dir.join(format!("new/added{i:03}.txt")), &buf[..n])?;
    }
    let victim = dir.join("src/m000");
    if victim.is_dir() {
        let mut names: Vec<_> = fs::read_dir(&victim)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<_>>()?;
        names.sort();
        if let Some(p) = names.first() {
            fs::remove_file(p)?;
        }
    }
    Ok(())
}

/// Chunks in one fixture index object (each stays well inside one fetch batch).
const FIXTURE_PER_OBJECT: usize = 200_000;

/// Bench fixture (AC-8 on the Mac): a synthetic chunk map of `chunks` chunks in packs of
/// `per_pack` chunks, written under `dir` in the store's own layout for `rclone copy` into an
/// existing repository. It is `v1/index/rfixture<seed>-<n>/000000.idx` objects of at most 200k
/// chunks each, and one one-byte object per pack. Every pack is listed with its real size and
/// `kind` checksum, so a load keeps every pack and every chunk, exactly as a repository of
/// that many packs would cost it. No fixture chunk is ever read: a restore reads only the
/// chunks of the run it restores. Returns (index objects, packs).
pub fn index_fixture(
    dir: &Path,
    chunks: usize,
    per_pack: usize,
    kind: crate::hash::HashKind,
    seed: u64,
) -> Result<(usize, usize)> {
    use crate::hash::ChunkId;
    use crate::repo::{ChunkRef, IdxObj, ObjRef};
    let per_pack = per_pack.max(1);
    let per_obj = (FIXTURE_PER_OBJECT / per_pack).max(1) * per_pack;
    let body = b"p";
    let hash = kind.digest(body);
    let (mut objects, mut packs, mut lo) = (0usize, 0usize, 0usize);
    while lo < chunks {
        let hi = (lo + per_obj).min(chunks);
        let refs: Vec<ObjRef> = (0..(hi - lo).div_ceil(per_pack))
            .map(|_| {
                let h = blake3::hash(format!("fixture {seed} pack {packs}").as_bytes()).to_hex();
                packs += 1;
                ObjRef {
                    key: format!("v1/packs/{}/{h}.pack", &h[..2]),
                    size: body.len() as u64,
                    hash: hash.clone(),
                }
            })
            .collect();
        for r in &refs {
            let p = dir.join(&r.key);
            fs::create_dir_all(p.parent().expect("a pack key has a parent"))?;
            fs::write(&p, body)?;
        }
        let run = format!("rfixture{seed:x}-{objects:05}");
        let idx = IdxObj {
            v: 1,
            engine: crate::repo::engine(),
            run: run.clone(),
            seq: 0,
            root: "fixture".into(),
            hash_kind: kind,
            packs: refs,
            chunks: (lo..hi)
                .map(|i| ChunkRef {
                    id: ChunkId::of(format!("fixture {seed} chunk {i}").as_bytes()),
                    p: ((i - lo) / per_pack) as u32,
                    len: 1,
                })
                .collect(),
        };
        let p = dir.join(crate::repo::idx_key(&run, 0));
        fs::create_dir_all(p.parent().expect("an index key has a parent"))?;
        fs::write(&p, crate::repo::encode(&idx)?)?;
        objects += 1;
        lo = hi;
    }
    Ok((objects, packs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_fixture_loads_whole_with_every_pack_kept() {
        use crate::hash::HashKind;
        use crate::repo::{Meter, Repo};
        use crate::transport::local::LocalDir;
        use crate::transport::RetryPolicy;
        let d = tempfile::tempdir().expect("tempdir");
        // 1000 chunks a pack: 200k-chunk objects, so three objects and 411 packs
        let (objs, packs) =
            index_fixture(d.path(), 410_017, 1000, HashKind::Sha1, 7).expect("fixture");
        assert_eq!((objs, packs), (3, 411));
        let t = std::sync::Arc::new(LocalDir::new(d.path(), HashKind::Sha1).expect("local"));
        let repo = Repo::new(t, 100_001, RetryPolicy::fast()).expect("repo");
        let idx = repo.list_index().expect("list index");
        let listed = repo.list_packs().expect("list packs");
        assert_eq!((idx.len(), listed.len()), (objs, packs));
        let mut m = Meter::new("test", 256 << 20);
        let (map, n, referenced) = repo
            .load_chunk_map(&idx, &listed, &mut m, None)
            .expect("load");
        assert_eq!((map.len(), n, referenced.len()), (410_017, objs, 0));
        assert_eq!(map.packs.len(), packs);
    }
    #[test]
    fn deterministic() {
        let (mut a, mut b) = (Rng::new(1), Rng::new(1));
        let (mut x, mut y) = ([0u8; 37], [0u8; 37]);
        a.fill(&mut x);
        b.fill(&mut y);
        assert_eq!(x, y);
        let mut t = vec![0u8; 1 << 16];
        Rng::new(2).fill_text(&mut t);
        let c = zstd::bulk::compress(&t, 3).expect("zstd");
        assert!(
            c.len() * 2 < t.len(),
            "text should compress: {} -> {}",
            t.len(),
            c.len()
        );
    }

    /// Every workload generates into an absent directory, twice with the same bytes, and the
    /// second-run change applies (the bench generates each workload this way).
    #[test]
    fn workloads_generate_and_mutate() {
        let t = tempfile::tempdir().expect("tmp");
        for (i, w) in [Workload::W1, Workload::W2, Workload::W3, Workload::Mac]
            .into_iter()
            .enumerate()
        {
            let a = t.path().join(format!("a{i}"));
            let b = t.path().join(format!("b{i}"));
            let na = generate(&a, w, 8 << 20, 7).expect("generate a");
            let nb = generate(&b, w, 8 << 20, 7).expect("generate b");
            assert_eq!(na, nb);
            assert!(na >= 1);
            if matches!(w, Workload::W1 | Workload::Mac) {
                mutate(&a, 7).expect("mutate");
                assert!(a.join("new/added199.txt").is_file());
            }
        }
    }
}
