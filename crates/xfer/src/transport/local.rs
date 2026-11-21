//! A directory acting as an object store, for tests and offline runs. It behaves like a store:
//! writes are atomic (tmp + rename), and stat/list report a checksum computed from the stored
//! bytes, as B2 (sha1) or S3 (md5) would.

use super::{check_key, RemoteObject, Transport};
use crate::error::{Error, Result};
use crate::hash::HashKind;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct LocalDir {
    root: PathBuf,
    kind: HashKind,
    delay: Duration,
}

impl LocalDir {
    pub fn new(root: impl AsRef<Path>, kind: HashKind) -> Result<LocalDir> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        Ok(LocalDir {
            root,
            kind,
            delay: Duration::ZERO,
        })
    }

    pub fn with_delay(mut self, d: Duration) -> LocalDir {
        self.delay = d;
        self
    }

    pub fn path_of(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    fn object(&self, key: &str, p: &Path) -> Result<RemoteObject> {
        // sized from its length first (see `util::read_bounded`): a listing hashes every object
        let f = fs::File::open(p)?;
        let len = f.metadata()?.len();
        let mut data = Vec::with_capacity(len.min(crate::util::READ_RESERVE_MAX) as usize);
        f.take(len).read_to_end(&mut data)?;
        Ok(RemoteObject {
            key: key.to_string(),
            size: data.len() as u64,
            hash: Some(self.kind.digest(&data)),
        })
    }
}

impl Transport for LocalDir {
    fn hash_kind(&self) -> Option<HashKind> {
        Some(self.kind)
    }

    fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        check_key(key)?;
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        let p = self.path_of(key);
        if let Some(d) = p.parent() {
            fs::create_dir_all(d)?;
        }
        let tmp = p.with_extension(format!("tmp-{}", crate::util::random_hex(6)));
        fs::write(&tmp, data)?;
        fs::rename(&tmp, &p)?;
        Ok(())
    }

    fn stat(&self, key: &str) -> Result<Option<RemoteObject>> {
        check_key(key)?;
        let p = self.path_of(key);
        if !p.is_file() {
            return Ok(None);
        }
        self.object(key, &p).map(Some)
    }

    fn list(&self, prefix: &str) -> Result<Vec<RemoteObject>> {
        let base = if prefix.is_empty() {
            self.root.clone()
        } else {
            check_key(prefix)?;
            self.path_of(prefix)
        };
        let mut out = Vec::new();
        let mut stack = vec![base];
        while let Some(d) = stack.pop() {
            let rd = match fs::read_dir(&d) {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            for e in rd {
                let e = e?;
                let p = e.path();
                let ft = e.file_type()?;
                if ft.is_dir() {
                    stack.push(p);
                } else if ft.is_file() {
                    let rel = p
                        .strip_prefix(&self.root)
                        .map_err(|_| Error::Bug("list outside root".into()))?;
                    let key = rel.to_string_lossy().replace('\\', "/");
                    if key.contains(".tmp-") {
                        continue;
                    }
                    out.push(self.object(&key, &p)?);
                }
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn get(&self, key: &str, max: u64) -> Result<Vec<u8>> {
        check_key(key)?;
        let f = fs::File::open(self.path_of(key))
            .map_err(|e| Error::Transport(format!("get {key}: {e}")))?;
        crate::util::read_bounded(f, max)?
            .ok_or_else(|| Error::corrupt(format!("{key} is larger than the {max} byte limit")))
    }

    fn describe(&self) -> String {
        format!("local:{} ({})", self.root.display(), self.kind.name())
    }
}
