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
