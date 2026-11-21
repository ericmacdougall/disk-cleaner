//! A fault-injecting transport wrapper for tests: lost responses, throttling, missing or wrong
//! checksums, and a transport that dies after N uploads (rcd death).

use super::{RemoteObject, Transport};
use crate::error::{Error, Result};
use crate::hash::HashKind;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HashFault {
    #[default]
    Pass,
    Drop,
    Empty,
    Corrupt,
}

#[derive(Default)]
pub struct FaultConfig {
    /// 0-based put numbers whose response is lost: the object is stored, the caller gets an error.
    pub lose_response: HashSet<u64>,
    /// The first N puts fail with a throttling error (nothing stored).
    pub throttle_first: u64,
    /// How stat and list report checksums for keys starting with `hash_prefix`.
    pub hash_fault: HashFault,
    pub hash_prefix: String,
    /// After this many successful puts every operation fails (the transport died).
    pub die_after_puts: Option<u64>,
    /// list() reports no checksums at all.
    pub list_without_hashes: bool,
}

pub struct Faulty {
    inner: Arc<dyn Transport>,
    cfg: Mutex<FaultConfig>,
    pub puts: AtomicU64,
    pub stored: AtomicU64,
    pub put_keys: Mutex<Vec<String>>,
}

impl Faulty {
    pub fn new(inner: Arc<dyn Transport>, cfg: FaultConfig) -> Faulty {
        Faulty {
            inner,
            cfg: Mutex::new(cfg),
            puts: AtomicU64::new(0),
            stored: AtomicU64::new(0),
            put_keys: Mutex::new(Vec::new()),
        }
    }

    pub fn configure(&self, f: impl FnOnce(&mut FaultConfig)) {
        f(&mut self.cfg.lock().expect("fault cfg"));
    }

    fn dead(&self) -> bool {
        let c = self.cfg.lock().expect("fault cfg");
        matches!(c.die_after_puts, Some(n) if self.stored.load(Ordering::SeqCst) >= n)
    }

    fn fault_hash(&self, o: RemoteObject) -> RemoteObject {
        let c = self.cfg.lock().expect("fault cfg");
        if !o.key.starts_with(&c.hash_prefix) {
            return o;
        }
        let hash = match c.hash_fault {
            HashFault::Pass => o.hash,
            HashFault::Drop => None,
            HashFault::Empty => Some(String::new()),
            HashFault::Corrupt => o.hash.map(|h| {
                let mut b = h.into_bytes();
                b[0] = if b[0] == b'0' { b'1' } else { b'0' };
                String::from_utf8(b).unwrap_or_default()
            }),
        };
        RemoteObject { hash, ..o }
    }
}

impl Transport for Faulty {
    fn hash_kind(&self) -> Option<HashKind> {
        self.inner.hash_kind()
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        if self.dead() {
            return Err(Error::Transport("transport died".into()));
        }
        let n = self.puts.fetch_add(1, Ordering::SeqCst);
        let (throttle, lose) = {
            let c = self.cfg.lock().expect("fault cfg");
            (n < c.throttle_first, c.lose_response.contains(&n))
        };
        if throttle {
            return Err(Error::Throttled("429 Too Many Requests (injected)".into()));
        }
        self.inner.put(key, data)?;
        self.stored.fetch_add(1, Ordering::SeqCst);
        self.put_keys.lock().expect("keys").push(key.to_string());
        if lose {
            return Err(Error::Transport(
                "connection reset after upload (injected lost response)".into(),
            ));
        }
        Ok(())
    }

    fn stat(&self, key: &str) -> Result<Option<RemoteObject>> {
        if self.dead() {
            return Err(Error::Transport("transport died".into()));
        }
        Ok(self.inner.stat(key)?.map(|o| self.fault_hash(o)))
    }

    fn list(&self, prefix: &str) -> Result<Vec<RemoteObject>> {
        if self.dead() {
            return Err(Error::Transport("transport died".into()));
        }
        let no_hash = self.cfg.lock().expect("fault cfg").list_without_hashes;
        Ok(self
            .inner
            .list(prefix)?
            .into_iter()
            .map(|o| self.fault_hash(o))
            .map(|o| {
                if no_hash {
                    RemoteObject { hash: None, ..o }
                } else {
                    o
                }
            })
            .collect())
    }

    fn get(&self, key: &str, max: u64) -> Result<Vec<u8>> {
        if self.dead() {
            return Err(Error::Transport("transport died".into()));
        }
        self.inner.get(key, max)
    }

    fn describe(&self) -> String {
        format!("faulty({})", self.inner.describe())
    }
}
