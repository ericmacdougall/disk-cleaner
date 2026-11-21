//! Small helpers: hex, base64, size parsing, randomness, time.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let v = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    for i in (0..b.len()).step_by(2) {
        out.push(v(b[i])? << 4 | v(b[i + 1])?);
    }
    Some(out)
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Parse "64Mi", "4G", "1.5GiB", "1000" (bytes). Binary and decimal suffixes are both accepted.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, suf) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult: f64 = match suf.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "ki" | "kib" => 1024.0,
        "m" | "mb" => 1e6,
        "mi" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" => 1e9,
        "gi" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => 1e12,
        "ti" | "tib" => 1024f64.powi(4),
        _ => return None,
    };
    if n.is_nan() || n < 0.0 {
        return None;
    }
    Some((n * mult) as u64)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 32 bytes from the OS. On unix this reads /dev/urandom; elsewhere it mixes the std hasher's
/// OS-seeded keys with the clock. Used for per-run credentials and ids, never for content.
pub fn random_bytes() -> [u8; 32] {
    #[cfg(unix)]
    {
        use std::io::Read;
        let mut b = [0u8; 32];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(&mut b).is_ok() {
                return b;
            }
        }
    }
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = blake3::Hasher::new();
    for i in 0..8u64 {
        let mut s = RandomState::new().build_hasher();
        s.write_u64(i);
        h.update(&s.finish().to_le_bytes());
    }
    h.update(&now_ms().to_le_bytes());
    h.update(&std::process::id().to_le_bytes());
    *h.finalize().as_bytes()
}

pub fn random_hex(nbytes: usize) -> String {
    hex(&random_bytes()[..nbytes.min(32)])
}

/// A sortable run id: 12 hex digits of unix ms, a dash and 8 random hex digits.
pub fn new_run_id() -> String {
    format!("r{:012x}-{}", now_ms(), random_hex(4))
}

pub fn human(n: u64) -> String {
    let f = n as f64;
    if f >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GiB", f / (1024.0 * 1024.0 * 1024.0))
    } else if f >= 1024.0 * 1024.0 {
        format!("{:.1} MiB", f / (1024.0 * 1024.0))
    } else if f >= 1024.0 {
        format!("{:.1} KiB", f / 1024.0)
    } else {
        format!("{n} B")
    }
}

/// Most bytes [`read_bounded`] reserves up front; a larger bound grows as it reads.
pub const READ_RESERVE_MAX: u64 = 256 << 20;

/// Read all of `r`, refusing more than `max` bytes (`Ok(None)` when it is larger). The buffer
/// is reserved for `max + 1` bytes before the first read (up to [`READ_RESERVE_MAX`]), so a
/// body is one allocation: a doubling `read_to_end` copies it at each step, and macOS keeps
/// every freed intermediate block resident (CI measured 7 MiB per fetch batch). Reserved pages
/// that are never written cost address space, not memory. Every transport reads through this.
pub fn read_bounded(r: impl std::io::Read, max: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read;
    let mut buf = Vec::with_capacity(max.saturating_add(1).min(READ_RESERVE_MAX) as usize);
    r.take(max.saturating_add(1)).read_to_end(&mut buf)?;
    Ok((buf.len() as u64 <= max).then_some(buf))
}

/// glibc raises its mmap threshold each time a large block is freed (up to 32 MiB), after
/// which 4 MiB segment buffers come from the heap and stay resident once freed: on t2-dev the
/// engine sat about 200 MiB above its live data. A fixed threshold keeps every buffer of 1 MiB
/// or more mmapped, so freeing it returns the memory. Process-wide; call once at start.
pub fn tune_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if !cfg!(feature = "broken-mem-plan") {
        // SAFETY: mallopt only sets an allocator parameter.
        unsafe {
            libc::mallopt(libc::M_MMAP_THRESHOLD, 1 << 20);
        }
    }
}

/// Hand freed heap memory back to the system now. macOS's allocator keeps freed blocks of the
/// sizes a decoded commit object uses resident until memory pressure, so without this every
/// object decoded while loading would add to the peak (CI measured 320 MiB on macOS where Linux
/// peaks at 150 MiB for the same restore). Linux needs nothing: blocks over the 1 MiB mmap
/// threshold set by [`tune_allocator`] go back on free.
pub fn release_free_memory() {
    #[cfg(target_os = "macos")]
    if !cfg!(feature = "broken-mem-plan") {
        extern "C" {
            fn malloc_zone_pressure_relief(
                zone: *mut libc::c_void,
                goal: libc::size_t,
            ) -> libc::size_t;
        }
        // SAFETY: a null zone means every zone, and goal 0 asks for as much as possible; the
        // call only returns free pages to the system.
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
}

/// Resident set size of a process in bytes, from /proc (Linux; None elsewhere or when gone).
pub fn rss_of(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kib: u64 = s
        .lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some(kib * 1024)
}

/// Peak resident set size of this process in bytes (0 when unknown).
pub fn peak_rss_bytes() -> u64 {
    #[cfg(unix)]
    {
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: getrusage writes into the zeroed struct we own.
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        if rc == 0 {
            let v = ru.ru_maxrss as u64;
            // Linux reports KiB, macOS bytes.
            return if cfg!(target_os = "macos") {
                v
            } else {
                v * 1024
            };
        }
    }
    0
}

/// Test-only diagnostics (feature `mem-trace`): print, tagged with `phase`, this process's peak
/// RSS and the allocator's own counts (bytes live now, the most ever live, and bytes it holds
/// from the system), so a measured peak can be split into live data and allocator retention.
/// Does nothing in a normal build.
pub fn mem_trace(phase: &str) {
    #[cfg(feature = "mem-trace")]
    {
        let (live, live_max, held) = malloc_counts();
        eprintln!(
            "mem-trace {phase}: peak {} KiB, malloc live {} KiB, live max {} KiB, held {} KiB",
            peak_rss_bytes() >> 10,
            live >> 10,
            live_max >> 10,
            held >> 10
        );
    }
    #[cfg(not(feature = "mem-trace"))]
    let _ = phase;
}

/// (live, most ever live, held from the system) in bytes; zeros where unknown.
#[cfg(feature = "mem-trace")]
fn malloc_counts() -> (usize, usize, usize) {
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        struct MallocStatistics {
            blocks_in_use: u32,
            size_in_use: usize,
            max_size_in_use: usize,
            size_allocated: usize,
        }
        extern "C" {
            fn malloc_zone_statistics(zone: *mut libc::c_void, stats: *mut MallocStatistics);
        }
        let mut s = MallocStatistics {
            blocks_in_use: 0,
            size_in_use: 0,
            max_size_in_use: 0,
            size_allocated: 0,
        };
        // SAFETY: a null zone sums every zone; the call fills the struct we own.
        unsafe { malloc_zone_statistics(std::ptr::null_mut(), &mut s) };
        let _ = s.blocks_in_use;
        (s.size_in_use, s.max_size_in_use, s.size_allocated)
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Linux: VmRSS now in place of live bytes (glibc has no cheap equivalent here)
        let rss = rss_of(std::process::id()).unwrap_or(0) as usize;
        (rss, 0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip_and_base64() {
        assert_eq!(hex(&[0, 255, 16]), "00ff10");
        assert_eq!(unhex("00ff10").unwrap(), vec![0, 255, 16]);
        assert!(unhex("0").is_none());
        assert!(unhex("zz").is_none());
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"f"), "Zg==");
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("64Mi"), Some(64 << 20));
        assert_eq!(parse_size("4G"), Some(4_000_000_000));
        assert_eq!(parse_size("1000"), Some(1000));
        assert_eq!(parse_size("1.5GiB"), Some(1536 << 20));
        assert_eq!(parse_size("x"), None);
    }
}
