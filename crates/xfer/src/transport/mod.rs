//! Object-store transports and the single verification choke point, [`verified_put`].

pub mod fault;
pub mod local;
pub mod rclone;

use crate::error::{Error, Result};
use crate::hash::HashKind;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// An object as the store reports it. `hash` is the store's own checksum of the stored bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteObject {
    pub key: String,
    pub size: u64,
    pub hash: Option<String>,
}

/// Every transport refuses keys that could escape the repository root or be read differently by
/// another backend: empty, absolute, `.`/`..` or empty components, backslashes, control bytes.
pub fn check_key(key: &str) -> Result<()> {
    if key.is_empty()
        || key.starts_with('/')
        || key.contains('\\')
        || key.bytes().any(|b| b < 0x20 || b == 0x7f)
        || key
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Err(Error::Refused(format!("bad object key {key:?}")));
    }
    Ok(())
}

/// Keys are relative to the repository root and use `/` separators.
pub trait Transport: Send + Sync {
    /// The checksum the store reports for stored objects, if any.
    fn hash_kind(&self) -> Option<HashKind>;
    fn put(&self, key: &str, data: &[u8]) -> Result<()>;
    fn stat(&self, key: &str) -> Result<Option<RemoteObject>>;
    /// Every object under `prefix` (recursive). A missing prefix is an empty list.
    fn list(&self, prefix: &str) -> Result<Vec<RemoteObject>>;
    /// The whole object, refusing more than `max` bytes.
    fn get(&self, key: &str, max: u64) -> Result<Vec<u8>>;
    fn describe(&self) -> String;
}

/// Request counters, kept by [`Metered`].
#[derive(Default, Debug)]
pub struct Counters {
    pub puts: AtomicU64,
    pub put_bytes: AtomicU64,
    pub stats: AtomicU64,
    pub lists: AtomicU64,
    pub gets: AtomicU64,
    pub get_bytes: AtomicU64,
    pub errors: AtomicU64,
}

impl Counters {
    pub fn snapshot(&self) -> CounterSnapshot {
        CounterSnapshot {
            puts: self.puts.load(Ordering::Relaxed),
            put_bytes: self.put_bytes.load(Ordering::Relaxed),
            stats: self.stats.load(Ordering::Relaxed),
            lists: self.lists.load(Ordering::Relaxed),
            gets: self.gets.load(Ordering::Relaxed),
            get_bytes: self.get_bytes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize, PartialEq, Eq)]
pub struct CounterSnapshot {
    pub puts: u64,
    pub put_bytes: u64,
    pub stats: u64,
    pub lists: u64,
    pub gets: u64,
    pub get_bytes: u64,
    pub errors: u64,
}

impl CounterSnapshot {
    pub fn requests(&self) -> u64 {
        self.puts + self.stats + self.lists + self.gets
    }
}

/// Counts every request that reaches the inner transport.
pub struct Metered {
    pub inner: Arc<dyn Transport>,
    pub counters: Arc<Counters>,
}

impl Metered {
    pub fn new(inner: Arc<dyn Transport>) -> Metered {
        Metered {
            inner,
            counters: Arc::new(Counters::default()),
        }
    }
    fn count<T>(&self, r: Result<T>) -> Result<T> {
        if r.is_err() {
            self.counters.errors.fetch_add(1, Ordering::Relaxed);
        }
        r
    }
}

impl Transport for Metered {
    fn hash_kind(&self) -> Option<HashKind> {
        self.inner.hash_kind()
    }
    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        self.counters.puts.fetch_add(1, Ordering::Relaxed);
        self.counters
            .put_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.count(self.inner.put(key, data))
    }
    fn stat(&self, key: &str) -> Result<Option<RemoteObject>> {
        self.counters.stats.fetch_add(1, Ordering::Relaxed);
        self.count(self.inner.stat(key))
    }
    fn list(&self, prefix: &str) -> Result<Vec<RemoteObject>> {
        self.counters.lists.fetch_add(1, Ordering::Relaxed);
        self.count(self.inner.list(prefix))
    }
    fn get(&self, key: &str, max: u64) -> Result<Vec<u8>> {
        self.counters.gets.fetch_add(1, Ordering::Relaxed);
        let r = self.count(self.inner.get(key, max));
        if let Ok(b) = &r {
            self.counters
                .get_bytes
                .fetch_add(b.len() as u64, Ordering::Relaxed);
        }
        r
    }
    fn describe(&self) -> String {
        self.inner.describe()
    }
}

/// What a verified upload proved: the store holds `size` bytes whose own checksum equals ours.
#[derive(Clone, Debug, PartialEq)]
pub struct Verified {
    pub key: String,
    pub size: u64,
    pub kind: HashKind,
    pub hash: String,
}

#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    pub retries: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            retries: 6,
            base: Duration::from_millis(500),
            cap: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    pub fn fast() -> Self {
        RetryPolicy {
            retries: 6,
            base: Duration::from_millis(1),
            cap: Duration::from_millis(5),
        }
    }
    fn delay(&self, attempt: u32) -> Duration {
        let exp = self.base.saturating_mul(1u32 << attempt.min(16));
        let d = exp.min(self.cap);
        // jitter: 50% to 100% of the delay
        let r = crate::util::random_bytes()[0] as u32;
        d.mul_f64(0.5 + (r as f64 / 255.0) * 0.5)
    }
}

/// Compare a store report with what we sent. Missing or empty hashes and S3 multipart ETags are
/// unverifiable: the store did not prove it holds our bytes.
pub fn check_remote(
    o: &RemoteObject,
    key: &str,
    size: u64,
    kind: HashKind,
    local: &str,
) -> Result<()> {
    // The broken variant skips the comparison; the verify_guard test must fail against it.
    // Never enabled in a release build.
    if cfg!(feature = "broken-verify") {
        return Ok(());
    }
    if o.size != size {
        return Err(Error::Mismatch {
            key: key.into(),
            why: format!("size {} != {}", o.size, size),
        });
    }
    let h = match o.hash.as_deref().map(str::trim) {
        Some(h) if !h.is_empty() => h,
        _ => {
            return Err(Error::Unverifiable {
                key: key.into(),
                why: "the store returned no checksum".into(),
            })
        }
    };
    if kind == HashKind::Md5 && h.contains('-') {
        return Err(Error::Unverifiable {
            key: key.into(),
            why: "multipart ETag is not an MD5 of the object".into(),
        });
    }
    if !h.eq_ignore_ascii_case(local) {
        return Err(Error::Mismatch {
            key: key.into(),
            why: format!("{} {} != local {}", kind.name(), h, local),
        });
    }
    Ok(())
}

/// Upload `data` under `key` and prove the store holds it, by the store's own checksum.
///
/// This is the only way the engine writes an object it will later rely on. A retry after an error
/// first stats the key, so a lost response (object stored, reply lost) is not uploaded twice.
/// `on_error` sees every failed attempt (the controller counts throttling there).
pub fn verified_put(
    t: &dyn Transport,
    key: &str,
    data: &[u8],
    policy: RetryPolicy,
    on_error: &dyn Fn(&Error),
) -> Result<Verified> {
    let kind = t.hash_kind().ok_or_else(|| Error::Unverifiable {
        key: key.into(),
        why: "the remote reports no supported checksum (sha1 or md5)".into(),
    })?;
    let local = kind.digest(data);
    let size = data.len() as u64;
    let done = || Verified {
        key: key.into(),
        size,
        kind,
        hash: local.clone(),
    };
    let mut attempt = 0u32;
    loop {
        if attempt > 0 && !cfg!(feature = "broken-retry") {
            match t.stat(key) {
                Ok(Some(o)) if check_remote(&o, key, size, kind, &local).is_ok() => {
                    return Ok(done())
                }
                Ok(_) => {}
                Err(e) => {
                    on_error(&e);
                    if !e.retryable() || attempt >= policy.retries {
                        return Err(e);
                    }
                    std::thread::sleep(policy.delay(attempt));
                    attempt += 1;
                    continue;
                }
            }
        }
        let r = t.put(key, data).and_then(|_| {
            let o = t
                .stat(key)?
                .ok_or_else(|| Error::Transport(format!("{key} missing right after upload")))?;
            check_remote(&o, key, size, kind, &local)
        });
        match r {
            Ok(()) => return Ok(done()),
            Err(e) => {
                on_error(&e);
                if !e.retryable() || attempt >= policy.retries {
                    return Err(e);
                }
                std::thread::sleep(policy.delay(attempt));
                attempt += 1;
            }
        }
    }
}

/// Open a transport from a repository string: `local:<dir>` (optionally `?hash=md5&delay_ms=N`)
/// or any rclone remote path, which starts a private rclone rcd.
pub fn open(repo: &str, rclone_bin: &str, max_pack: u64) -> Result<Arc<dyn Transport>> {
    open_limited(repo, rclone_bin, max_pack, None)
}

/// As [`open`]; an rclone rcd runs with its Go heap limited to `rcd_heap` bytes (the budget's
/// [`crate::repo::Budget::rcd_heap`]), so its memory stays inside `--mem-budget`.
pub fn open_limited(
    repo: &str,
    rclone_bin: &str,
    max_pack: u64,
    rcd_heap: Option<u64>,
) -> Result<Arc<dyn Transport>> {
    if repo.contains('\n') || repo.contains('\0') {
        return Err(Error::Refused(
            "repository string contains a control character".into(),
        ));
    }
    if let Some(rest) = repo.strip_prefix("local:") {
        let (dir, q) = rest.split_once('?').unwrap_or((rest, ""));
        let mut kind = HashKind::Sha1;
        let mut delay = 0u64;
        for kv in q.split('&').filter(|s| !s.is_empty()) {
            match kv.split_once('=') {
                Some(("hash", v)) => {
                    kind = HashKind::parse(v).ok_or_else(|| Error::Refused(format!("hash {v}")))?
                }
                Some(("delay_ms", v)) => {
                    delay = v
                        .parse()
                        .map_err(|_| Error::Refused(format!("delay_ms {v}")))?
                }
                _ => return Err(Error::Refused(format!("unknown local option {kv}"))),
            }
        }
        let t = local::LocalDir::new(dir, kind)?.with_delay(Duration::from_millis(delay));
        return Ok(Arc::new(t));
    }
    Ok(Arc::new(rclone::RcloneRcd::start_limited(
        rclone_bin, repo, max_pack, rcd_heap,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_remote_rules() {
        let o = |h: Option<&str>, size| RemoteObject {
            key: "k".into(),
            size,
            hash: h.map(String::from),
        };
        let l = HashKind::Sha1.digest(b"x");
        assert!(check_remote(&o(Some(&l), 1), "k", 1, HashKind::Sha1, &l).is_ok());
        assert!(check_remote(&o(Some(&l.to_uppercase()), 1), "k", 1, HashKind::Sha1, &l).is_ok());
        assert!(matches!(
            check_remote(&o(None, 1), "k", 1, HashKind::Sha1, &l),
            Err(Error::Unverifiable { .. })
        ));
        assert!(matches!(
            check_remote(&o(Some(""), 1), "k", 1, HashKind::Sha1, &l),
            Err(Error::Unverifiable { .. })
        ));
        assert!(matches!(
            check_remote(&o(Some(&l), 2), "k", 1, HashKind::Sha1, &l),
            Err(Error::Mismatch { .. })
        ));
        assert!(matches!(
            check_remote(&o(Some("abc"), 1), "k", 1, HashKind::Sha1, &l),
            Err(Error::Mismatch { .. })
        ));
        let m = HashKind::Md5.digest(b"x");
        assert!(matches!(
            check_remote(
                &o(Some("d41d8cd98f00b204e9800998ecf8427e-2"), 1),
                "k",
                1,
                HashKind::Md5,
                &m
            ),
            Err(Error::Unverifiable { .. })
        ));
    }
}
