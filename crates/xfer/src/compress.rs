//! Content classes, the entropy probe and the rate-matched zstd level chooser.
//!
//! For compressible segments the chooser keeps, per zstd level L, EWMAs of per-thread input
//! throughput c(L) (bytes/s) and output ratio r(L). With the link goodput G (bytes/s, measured by
//! the upload controller) and Nc compression threads it picks the level maximising source bytes
//! backed up per second: T(L) = min(Nc * c(L), G / r(L)). Neighbouring levels are explored on a
//! small sample of every 16th segment so the estimates track the data.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// Known compressible by extension (text, code, data formats).
    Compressible,
    Unknown,
    /// Known incompressible by extension (archives, media).
    Incompressible,
}

const INCOMPRESSIBLE: &[&str] = &[
    "zip", "gz", "tgz", "xz", "txz", "zst", "zstd", "bz2", "tbz", "7z", "rar", "lz4", "br", "jpg",
    "jpeg", "png", "gif", "webp", "heic", "heif", "avif", "mp4", "m4v", "mov", "mkv", "webm",
    "avi", "mp3", "aac", "m4a", "ogg", "opus", "flac", "woff", "woff2", "jar", "apk", "ipa", "dmg",
    "whl", "nupkg", "crx", "xpi", "pack", "idx",
];

const COMPRESSIBLE: &[&str] = &[
    "txt", "md", "rst", "rs", "py", "js", "mjs", "cjs", "ts", "tsx", "jsx", "json", "jsonl",
    "ndjson", "csv", "tsv", "log", "html", "htm", "css", "scss", "xml", "yaml", "yml", "toml",
    "ini", "cfg", "c", "h", "cc", "cpp", "hpp", "go", "java", "kt", "swift", "rb", "php", "sh",
    "bash", "zsh", "fish", "sql", "svg", "ipynb", "lock", "tex", "org", "proto", "graphql", "vue",
    "svelte", "lua", "pl", "r", "scala", "cs", "fs", "ex", "exs", "erl", "hs", "ml", "dart", "tf",
];

pub fn class_of(name: &[u8]) -> Class {
    let ext = match name.iter().rposition(|&b| b == b'.') {
        Some(i) if i + 1 < name.len() => &name[i + 1..],
        _ => return Class::Unknown,
    };
    let ext = String::from_utf8_lossy(ext).to_ascii_lowercase();
    if INCOMPRESSIBLE.contains(&ext.as_str()) {
        Class::Incompressible
    } else if COMPRESSIBLE.contains(&ext.as_str()) {
        Class::Compressible
    } else {
        Class::Unknown
    }
}

const PROBE: usize = 16 << 10;

/// True when a zstd level-1 pass over up to two 16 KiB samples saves at least 3%.
pub fn probe_compressible(data: &[u8]) -> bool {
    let sample = |s: &[u8]| -> bool {
        match zstd::bulk::compress(s, 1) {
            Ok(z) => (z.len() as f64) < s.len() as f64 * 0.97,
            Err(_) => false,
        }
    };
    if data.len() <= PROBE {
        return data.len() >= 64 && sample(data);
    }
    let mid = data.len() / 2;
    sample(&data[..PROBE]) || sample(&data[mid..(mid + PROBE).min(data.len())])
}

pub const LEVELS: &[i32] = &[1, 2, 3, 4, 6, 8, 10, 13, 16, 19];
const DEFAULT_IDX: usize = 2; // level 3

/// zstd's workspace in MiB for one segment of up to 4 MiB at each of [`LEVELS`], rounded up from
/// `ZSTD_sizeof_CCtx` after a one-shot compression (measured with zstd 1.5 on t2-dev: 0.6, 0.8,
/// 1.3, 2.6, 3.1, 5.8, 21.5, 34.1, 34.3 and 51.6 MiB; `cctx_table_covers_zstd` re-checks it).
pub const CCTX_MIB: [u64; 10] = [1, 1, 2, 3, 4, 6, 22, 35, 35, 52];
/// The output buffer of one compression (the bound of a 4 MiB segment), in MiB.
const OUT_MIB: u64 = 5;

/// Bytes one compression at level index `i` holds while it runs.
pub fn cost(i: usize) -> u64 {
    (CCTX_MIB[i] + OUT_MIB) << 20
}

/// The smallest compression share: one compression at level 3.
pub fn min_share() -> u64 {
    cost(DEFAULT_IDX)
}

/// A byte semaphore over the compression share.
struct Share {
    cap: u64,
    used: Mutex<u64>,
    cv: std::sync::Condvar,
}

impl Share {
    fn acquire(&self, n: u64) {
        let mut u = self.used.lock().expect("share lock");
        while *u + n > self.cap {
            u = self.cv.wait(u).expect("share wait");
        }
        *u += n;
    }
    fn try_acquire(&self, n: u64) -> bool {
        let mut u = self.used.lock().expect("share lock");
        if *u + n > self.cap {
            return false;
        }
        *u += n;
        true
    }
    fn release(&self, n: u64) {
        *self.used.lock().expect("share lock") -= n;
        self.cv.notify_all();
    }
}

#[derive(Clone, Copy, Default)]
struct Est {
    cps: f64,
    ratio: f64,
    n: u32,
}

impl Est {
    fn observe(&mut self, cps: f64, ratio: f64) {
        const A: f64 = 0.3;
        if self.n == 0 {
            self.cps = cps;
            self.ratio = ratio;
        } else {
            self.cps = A * cps + (1.0 - A) * self.cps;
            self.ratio = A * ratio + (1.0 - A) * self.ratio;
        }
        self.n = self.n.saturating_add(1);
    }
}

struct State {
    est: Vec<Est>,
    cur: usize,
    segments: u64,
    histogram: Vec<u64>,
}

/// Shared by every packer thread.
pub struct RateMatcher {
    fixed: Option<i32>,
    threads: usize,
    goodput: Arc<AtomicU64>,
    st: Mutex<State>,
    share: Share,
    /// the highest level index one compression fits in the share
    top: usize,
}

pub struct Compressed {
    pub level: i32,
    pub data: Vec<u8>,
}

impl RateMatcher {
    /// `share` is the budget's compression share Z: each compression holds its level's
    /// [`cost`] from it while it runs, so a higher level runs on fewer threads at once, and a
    /// level whose single compression does not fit is never used (a fixed level is lowered).
    pub fn new(fixed: Option<i32>, threads: usize, goodput: Arc<AtomicU64>, share: u64) -> Self {
        let cap = if cfg!(feature = "broken-mem-plan") {
            u64::MAX / 4 // the broken variant: no compression share at all
        } else {
            share.max(min_share())
        };
        let top = (0..LEVELS.len())
            .rev()
            .find(|&i| cost(i) <= cap)
            .unwrap_or(0);
        RateMatcher {
            fixed,
            threads: threads.max(1),
            goodput,
            st: Mutex::new(State {
                est: vec![Est::default(); LEVELS.len()],
                cur: DEFAULT_IDX.min(top),
                segments: 0,
                histogram: vec![0; LEVELS.len()],
            }),
            share: Share {
                cap,
                used: Mutex::new(0),
                cv: std::sync::Condvar::new(),
            },
            top,
        }
    }

    /// Compressions at level index `i` that fit in the share at once.
    fn parallel(&self, i: usize) -> usize {
        ((self.share.cap / cost(i)) as usize).min(self.threads)
    }

    /// Levels used so far, as (level, segments).
    pub fn histogram(&self) -> Vec<(i32, u64)> {
        let st = self.st.lock().expect("rate matcher lock");
        LEVELS
            .iter()
            .zip(&st.histogram)
            .filter(|(_, &n)| n > 0)
            .map(|(&l, &n)| (l, n))
            .collect()
    }

    /// Pick the level index maximising T(L) from current estimates.
    fn choose(&self, st: &State) -> usize {
        let g = self.goodput.load(Ordering::Relaxed) as f64;
        if g <= 0.0 {
            return st.cur;
        }
        let mut best = st.cur.min(self.top);
        let mut best_t = -1.0;
        for (i, e) in st.est.iter().enumerate().take(self.top + 1) {
            if e.n == 0 || e.ratio <= 0.0 {
                continue;
            }
            let nc = self.parallel(i) as f64;
            let t = (nc * e.cps).min(g / e.ratio);
            // prefer the higher level on (near) ties: same speed, better ratio
            if t > best_t * 1.02 || (t >= best_t * 0.98 && i > best) {
                best_t = t;
                best = i;
            }
        }
        best
    }

    /// Compress one segment at the chosen level, update estimates, and occasionally probe the
    /// neighbouring levels on a 256 KiB sample.
    pub fn compress(&self, raw: &[u8]) -> Compressed {
        let (idx, probe) = {
            let mut st = self.st.lock().expect("rate matcher lock");
            st.segments += 1;
            let idx = match self.fixed {
                Some(l) => LEVELS
                    .iter()
                    .position(|&x| x >= l)
                    .unwrap_or(LEVELS.len() - 1),
                None => {
                    let c = self.choose(&st);
                    st.cur = c;
                    c
                }
            };
            let idx = idx.min(self.top);
            st.histogram[idx] += 1;
            (idx, self.fixed.is_none() && (st.segments % 16 == 1))
        };
        // a fixed level above what the share allows is lowered to the highest that fits
        let level = match self.fixed {
            Some(l) if LEVELS[idx] >= l => l,
            _ => LEVELS[idx],
        };
        let held = cost(idx);
        self.share.acquire(held);
        let t0 = Instant::now();
        let data = zstd::bulk::compress(raw, level).unwrap_or_else(|_| raw.to_vec());
        self.record(idx, raw.len(), data.len(), t0.elapsed().as_secs_f64());
        if probe && raw.len() >= 64 << 10 {
            // neighbouring levels on a 256 KiB sample; a level up runs only if its extra
            // workspace is free in the share right now
            let sample = &raw[..raw.len().min(256 << 10)];
            for n in [idx.checked_sub(1), Some(idx + 1)].into_iter().flatten() {
                if n > self.top {
                    continue;
                }
                let extra = cost(n).saturating_sub(held);
                if extra > 0 && !self.share.try_acquire(extra) {
                    continue;
                }
                let t = Instant::now();
                if let Ok(z) = zstd::bulk::compress(sample, LEVELS[n]) {
                    self.record(n, sample.len(), z.len(), t.elapsed().as_secs_f64());
                }
                self.share.release(extra);
            }
        }
        self.share.release(held);
        Compressed { level, data }
    }

    fn record(&self, idx: usize, inb: usize, outb: usize, secs: f64) {
        if inb == 0 {
            return;
        }
        let cps = inb as f64 / secs.max(1e-6);
        let ratio = (outb as f64 / inb as f64).clamp(0.001, 1.5);
        self.st.lock().expect("rate matcher lock").est[idx].observe(cps, ratio);
    }

    #[cfg(test)]
    fn seed(&self, idx: usize, cps: f64, ratio: f64) {
        self.st.lock().unwrap().est[idx].observe(cps, ratio);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes() {
        assert_eq!(class_of(b"a/b/data.zip"), Class::Incompressible);
        assert_eq!(class_of(b"main.RS"), Class::Compressible);
        assert_eq!(class_of(b"weights.bin"), Class::Unknown);
        assert_eq!(class_of(b"Makefile"), Class::Unknown);
    }

    #[test]
    fn probe_tells_text_from_noise() {
        let text = b"fn main() { println!(\"hello\"); }\n".repeat(1000);
        assert!(probe_compressible(&text));
        let mut noise = vec![0u8; 64 << 10];
        crate::gen::Rng::new(1).fill(&mut noise);
        assert!(!probe_compressible(&noise));
    }

    #[test]
    fn slow_link_picks_a_higher_level_than_a_fast_link() {
        // c(L) falls and the ratio improves with level, as zstd does.
        let seed = |m: &RateMatcher| {
            for (i, &l) in LEVELS.iter().enumerate() {
                let cps = 400e6 / (1.0 + l as f64 * 0.6);
                let ratio = 0.40 - 0.01 * l as f64;
                m.seed(i, cps, ratio);
            }
        };
        let big = 1 << 30;
        let slow = RateMatcher::new(None, 4, Arc::new(AtomicU64::new(12_000_000)), big);
        seed(&slow);
        let fast = RateMatcher::new(None, 4, Arc::new(AtomicU64::new(2_000_000_000)), big);
        seed(&fast);
        let ls = LEVELS[slow.choose(&slow.st.lock().unwrap())];
        let lf = LEVELS[fast.choose(&fast.st.lock().unwrap())];
        assert!(
            ls > lf,
            "slow link level {ls} should exceed fast link level {lf}"
        );
        assert!(lf <= 3);
    }

    /// zstd's workspace for one 4 MiB segment at `level`, as `ZSTD_sizeof_CCtx` reports it after
    /// a one-shot compression with a fresh context (what `zstd::bulk::compress` does).
    fn cctx_bytes(level: i32, raw: &[u8]) -> usize {
        use zstd::zstd_safe::{CCtx, CParameter};
        let mut c = CCtx::create();
        c.set_parameter(CParameter::CompressionLevel(level))
            .expect("level");
        let mut out = Vec::with_capacity(zstd::zstd_safe::compress_bound(raw.len()));
        c.compress2(&mut out, raw).expect("compress");
        c.sizeof()
    }

    fn segment() -> Vec<u8> {
        // half text-like, half noise: a typical mixed segment of SEG_MAX bytes
        let mut raw = b"let x = compute(a, b); // comment 0123456789\n".repeat(48_000);
        raw.truncate(2 << 20);
        let mut noise = vec![0u8; 2 << 20];
        crate::gen::Rng::new(3).fill(&mut noise);
        raw.extend_from_slice(&noise);
        raw
    }

    /// The table the compression share is charged from must not understate zstd's workspace.
    #[test]
    fn cctx_table_covers_zstd() {
        let raw = segment();
        for &l in &[1, 3, 8, 19] {
            let i = LEVELS.iter().position(|&x| x == l).expect("level");
            let got = cctx_bytes(l, &raw) as u64;
            assert!(
                got <= CCTX_MIB[i] << 20,
                "level {l}: zstd used {got} bytes, the table says {} MiB",
                CCTX_MIB[i]
            );
        }
    }

    /// A share too small for a level keeps it out: the chooser never picks it, a fixed level
    /// above it is lowered, and higher levels run on fewer threads at once.
    #[test]
    fn the_compression_share_bounds_levels_and_threads() {
        let g = || Arc::new(AtomicU64::new(1_000_000));
        let m = RateMatcher::new(Some(19), 8, g(), 32 << 20);
        let top = LEVELS[m.top];
        assert_eq!(
            top, 10,
            "32 MiB fits one level-10 compression (22 + 5 MiB), not level 13"
        );
        let raw = segment();
        assert_eq!(m.compress(&raw).level, 10, "a fixed level 19 is lowered");
        assert_eq!(m.parallel(LEVELS.iter().position(|&l| l == 3).unwrap()), 4);
        assert_eq!(m.parallel(m.top), 1);
        // the chooser, seeded so that level 19 would win on a slow link, stays at or below top
        let s = RateMatcher::new(None, 8, g(), 32 << 20);
        for (i, &l) in LEVELS.iter().enumerate() {
            s.seed(i, 400e6 / (1.0 + l as f64), 0.5 - 0.02 * l as f64);
        }
        let c = s.choose(&s.st.lock().unwrap());
        assert!(
            c <= s.top,
            "chose level {} above the share's top",
            LEVELS[c]
        );
    }

    #[test]
    #[ignore = "prints the measured table; run with --ignored"]
    fn print_cctx_sizes() {
        let raw = segment();
        for &l in LEVELS {
            eprintln!("CCTX level {l}: {} bytes", cctx_bytes(l, &raw));
        }
    }
}
