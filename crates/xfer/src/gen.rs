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
    /// Duplicate-heavy mixed tree: text, binary, exact duplicates, near-duplicate large files,
    /// hard links and symlinks.
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

fn write_stream(p: &Path, total: u64, rng: &mut Rng, text: bool) -> Result<()> {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d)?;
    }
    let mut f = std::io::BufWriter::with_capacity(1 << 20, fs::File::create(p)?);
    let mut buf = vec![0u8; 1 << 20];
    let mut left = total;
    while left > 0 {
        let n = left.min(buf.len() as u64) as usize;
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

/// Write a large file made of 1 MiB blocks drawn from a fixed pool, so most of it repeats other
/// regions (inside the file) and the other copies (across files) at unaligned offsets.
fn write_repetitive(p: &Path, total: u64, pool: &[Vec<u8>], rng: &mut Rng) -> Result<()> {
    if let Some(d) = p.parent() {
        fs::create_dir_all(d)?;
    }
    let mut f = std::io::BufWriter::with_capacity(1 << 20, fs::File::create(p)?);
    let mut left = total;
    let mut noise = vec![0u8; 4096];
    while left > 0 {
        let b = &pool[rng.below(pool.len() as u64) as usize];
        let n = left.min(b.len() as u64) as usize;
        f.write_all(&b[..n])?;
        left -= n as u64;
        if left > 0 && rng.below(4) == 0 {
            // a small unique insertion shifts every later boundary (fixed-size chunking would lose
            // all dedup after this point; CDC resynchronises)
            let k = (rng.below(4096) as usize + 1).min(left as usize);
            rng.fill(&mut noise[..k]);
            f.write_all(&noise[..k])?;
            left -= k as u64;
        }
    }
    f.flush()?;
    Ok(())
}

/// Generate a workload under `dir` scaled to roughly `total` bytes. Returns the file count.
pub fn generate(dir: &Path, w: Workload, total: u64, seed: u64) -> Result<u64> {
    fs::create_dir_all(dir)?;
    let mut rng = Rng::new(seed);
    let mut files = 0u64;
    match w {
        Workload::W2 => {
            write_stream(&dir.join("big.bin"), total, &mut rng, false)?;
            files = 1;
        }
        Workload::W3 => {
            // 4 KiB to 64 KiB files, 60% text, spread over 256 directories
            let mut written = 0u64;
            let mut buf = vec![0u8; 64 << 10];
            while written < total {
                let n = (4096 + rng.below(60 << 10)) as usize;
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
            // shares of the total: 30% text, 20% unique binary, 40% repetitive large files with
            // copies, 10% small files; plus exact duplicate copies, hard links and symlinks.
            let text = total * 3 / 10;
            let bin = total / 5;
            let rep = total * 2 / 5;
            let small = total / 10;
            let nbig = if w == Workload::Mac { 2 } else { 4 };
            for i in 0..nbig {
                write_stream(
                    &dir.join(format!("logs/log{i}.txt")),
                    text / nbig,
                    &mut rng,
                    true,
                )?;
                files += 1;
            }
            for i in 0..nbig {
                write_stream(
                    &dir.join(format!("media/clip{i}.mp4")),
                    bin / nbig,
                    &mut rng,
                    false,
                )?;
                files += 1;
            }
            let mut pool = Vec::new();
            for _ in 0..48 {
                let mut b = vec![0u8; 1 << 20];
                if rng.below(2) == 0 {
                    rng.fill(&mut b);
                } else {
                    rng.fill_text(&mut b);
                }
                pool.push(b);
            }
            // two "VM images" that share most content, then exact copies of the first one
            let img = rep / 4;
            write_repetitive(&dir.join("vm/disk-a.img"), img, &pool, &mut rng)?;
            write_repetitive(&dir.join("vm/disk-b.img"), img, &pool, &mut rng)?;
            fs::copy(
                dir.join("vm/disk-a.img"),
                dir.join("copies/disk-a copy.img"),
            )?;
            fs::copy(dir.join("vm/disk-a.img"), dir.join("copies/old/disk-a.img")).or_else(
                |_| {
                    fs::create_dir_all(dir.join("copies/old"))?;
                    fs::copy(dir.join("vm/disk-a.img"), dir.join("copies/old/disk-a.img"))
                },
            )?;
            files += 4;
            let mut written = 0u64;
            let mut buf = vec![0u8; 64 << 10];
            while written < small {
                let n = (512 + rng.below(32 << 10)) as usize;
                rng.fill_text(&mut buf[..n]);
                write_file(
                    &dir.join(format!("src/m{:03}/f{:06}.rs", files % 97, files)),
                    &buf[..n],
                )?;
                written += n as u64;
                files += 1;
            }
            // duplicated small files (a copied project folder)
            let src = dir.join("src/m001");
            if src.is_dir() {
                for e in fs::read_dir(&src)? {
                    let e = e?;
                    let to = dir.join("src-backup/m001").join(e.file_name());
                    fs::create_dir_all(to.parent().expect("parent"))?;
                    fs::copy(e.path(), &to)?;
                    files += 1;
                }
            }
            #[cfg(unix)]
            {
                fs::create_dir_all(dir.join("links"))?;
                fs::hard_link(dir.join("logs/log0.txt"), dir.join("links/log0-hard.txt"))?;
                std::os::unix::fs::symlink("../logs/log1.txt", dir.join("links/log1-sym.txt"))?;
                std::os::unix::fs::symlink("does-not-exist", dir.join("links/dangling"))?;
                files += 3;
            }
        }
    }
    Ok(files)
}

/// A small change for the second-run workload (W4): append to one text file, rewrite 64 KiB in
/// the middle of one large file, add one new file and delete one small file.
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
    let img = dir.join("vm/disk-b.img");
    if img.is_file() {
        use std::io::{Seek, SeekFrom};
        let len = fs::metadata(&img)?.len();
        let mut f = fs::OpenOptions::new().write(true).open(&img)?;
        f.seek(SeekFrom::Start(len / 2))?;
        rng.fill(&mut buf);
        f.write_all(&buf)?;
    }
    rng.fill_text(&mut buf[..8192]);
    write_file(&dir.join("new/added.txt"), &buf[..8192])?;
    let victim = dir.join("src/m000");
    if victim.is_dir() {
        if let Some(e) = fs::read_dir(&victim)?.next() {
            fs::remove_file(e?.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
