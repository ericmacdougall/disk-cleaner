//! Receipts: the contract between this engine and a deleter.
//!
//! One JSONL line (`"v":1`) per path, written only after the pack(s) holding the path's bytes and
//! both commit objects that list it (`.idx` and `.files`) were uploaded and verified by the
//! store's checksum. A torn last line is ignored by [`read_receipts`].
//!
//! A deleter must call [`Receipt::deletable`] immediately before each unlink. It revalidates the
//! identity tuple with `lstat` (so a modified, replaced or symlink-swapped path is refused),
//! refuses incomplete metadata, unknown receipt versions and denied engine builds.

use crate::error::{Error, Result};
use crate::meta::{stat_of, Identity, Kind};
use crate::repo::{enc_bytes, Entry, ObjRef, RECEIPT_V};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Receipt {
    pub v: u32,
    pub engine: String,
    pub repo: String,
    pub run: String,
    /// The chunk map of the commit that made this path restorable.
    pub idx: ObjRef,
    /// The file table that lists this path.
    pub files: ObjRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_hex: Option<String>,
    pub kind: Kind,
    pub id: Identity,
    pub mode: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blake3: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_group: Option<String>,
    pub alloc_bytes: u64,
    pub meta_incomplete: bool,
}

/// What the consumer accepts.
#[derive(Clone, Debug)]
pub struct Policy {
    /// Receipt versions accepted.
    pub versions: Vec<u32>,
    /// Engine builds refused (prefix match on `engine`).
    pub deny: Vec<String>,
    /// Accept receipts from builds without a recorded commit (`+unknown`).
    pub allow_unknown_build: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            versions: vec![RECEIPT_V],
            deny: Vec::new(),
            allow_unknown_build: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    UnknownVersion(u32),
    DeniedEngine(String),
    UnknownBuild,
    MetaIncomplete,
    Missing,
    KindChanged,
    IdentityChanged(&'static str),
    BadPath,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deletable {
    Yes,
    No(Why),
}

impl Receipt {
    pub fn from_entry(
        e: &Entry,
        repo: &str,
        run: &str,
        engine: &str,
        idx: &ObjRef,
        files: &ObjRef,
    ) -> Receipt {
        Receipt {
            v: RECEIPT_V,
            engine: engine.to_string(),
            repo: repo.to_string(),
            run: run.to_string(),
            idx: idx.clone(),
            files: files.clone(),
            path: e.path.clone(),
            path_hex: e.path_hex.clone(),
            kind: e.kind,
            id: e.id,
            mode: e.mode,
            blake3: e.blake3.clone(),
            link_group: e.group.clone(),
            alloc_bytes: e.alloc,
            meta_incomplete: e.meta_incomplete,
        }
    }

    pub fn rel(&self) -> Result<Vec<u8>> {
        crate::repo::dec_bytes(&self.path, &self.path_hex)?
            .ok_or_else(|| Error::corrupt("receipt without a path"))
    }

    /// The identity-only check: an `lstat` of the path under `root` (no follow) gives the same
    /// kind and the same identity tuple.
    pub fn still_matches(&self, root: &Path) -> std::result::Result<(), Why> {
        let rel = self.rel().map_err(|_| Why::BadPath)?;
        crate::walk::validate_rel(&rel).map_err(|_| Why::BadPath)?;
        let p = root.join(crate::walk::rel_path(&rel));
        let md = std::fs::symlink_metadata(&p).map_err(|_| Why::Missing)?;
        let st = stat_of(&md);
        let want = match self.kind {
            Kind::Hardlink => Kind::File,
            k => k,
        };
        if st.kind != Ok(want) {
            return Err(Why::KindChanged);
        }
        let (a, b) = (&st.id, &self.id);
        if a.dev != b.dev || a.ino != b.ino {
            return Err(Why::IdentityChanged("replaced (new inode)"));
        }
        if want != Kind::Dir {
            if a.size != b.size {
                return Err(Why::IdentityChanged("size"));
            }
            if a.mtime_ns != b.mtime_ns {
                return Err(Why::IdentityChanged("mtime"));
            }
            if a.ctime_ns != b.ctime_ns {
                return Err(Why::IdentityChanged("ctime"));
            }
            if a.nlink != b.nlink {
                return Err(Why::IdentityChanged("hard-link count"));
            }
        }
        Ok(())
    }

    /// The deleter's mandatory check, to call immediately before unlinking this path.
    pub fn deletable(&self, root: &Path, policy: &Policy) -> Result<Deletable> {
        if !policy.versions.contains(&self.v) {
            return Ok(Deletable::No(Why::UnknownVersion(self.v)));
        }
        if let Some(d) = policy
            .deny
            .iter()
            .find(|d| self.engine.starts_with(d.as_str()))
        {
            return Ok(Deletable::No(Why::DeniedEngine(d.clone())));
        }
        if self.engine.ends_with("+unknown") && !policy.allow_unknown_build {
            return Ok(Deletable::No(Why::UnknownBuild));
        }
        if self.meta_incomplete {
            return Ok(Deletable::No(Why::MetaIncomplete));
        }
        Ok(match self.still_matches(root) {
            Ok(()) => Deletable::Yes,
            Err(w) => Deletable::No(w),
        })
    }
}

/// The check for a hard-link group. Unlinking one link changes the inode's ctime and link count,
/// so after the first unlink the other members' receipts no longer match. A deleter therefore
/// checks the whole group once with this function, then unlinks every member. It returns `Yes`
/// only when every member is deletable, they share one inode and group id, and the receipts
/// cover all `nlink` links (no link outside the input still holds the data, which is the
/// conservative reading: a link outside the input means the group is not freed).
pub fn deletable_group(members: &[&Receipt], root: &Path, policy: &Policy) -> Result<Deletable> {
    let Some(first) = members.first() else {
        return Ok(Deletable::No(Why::Missing));
    };
    let g = &first.link_group;
    if g.is_none() && members.len() > 1 {
        return Ok(Deletable::No(Why::IdentityChanged("not a hard-link group")));
    }
    for m in members {
        if &m.link_group != g || m.id.dev != first.id.dev || m.id.ino != first.id.ino {
            return Ok(Deletable::No(Why::IdentityChanged("group members differ")));
        }
        match m.deletable(root, policy)? {
            Deletable::Yes => {}
            no => return Ok(no),
        }
    }
    if (members.len() as u64) < first.id.nlink {
        return Ok(Deletable::No(Why::IdentityChanged(
            "not every hard link of the group is receipted",
        )));
    }
    Ok(Deletable::Yes)
}

/// Appends receipts; each commit's lines go out in one write, then fsync.
pub struct Writer {
    f: Option<std::fs::File>,
    pub written: u64,
}

impl Writer {
    pub fn open(path: Option<&Path>) -> Result<Writer> {
        let f = match path {
            Some(p) => Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)?,
            ),
            None => None,
        };
        Ok(Writer { f, written: 0 })
    }

    pub fn write_batch(&mut self, rs: &[Receipt]) -> Result<()> {
        let Some(f) = self.f.as_mut() else {
            self.written += rs.len() as u64;
            return Ok(());
        };
        let mut buf = Vec::with_capacity(rs.len() * 320);
        for r in rs {
            serde_json::to_writer(&mut buf, r).map_err(|e| Error::Bug(format!("receipt: {e}")))?;
            buf.push(b'\n');
        }
        f.write_all(&buf)?;
        f.sync_data()?;
        self.written += rs.len() as u64;
        Ok(())
    }
}

/// Read a receipts file; a torn (unterminated or unparsable) last line is ignored.
pub fn read_receipts(path: &Path) -> Result<Vec<Receipt>> {
    let f = std::fs::File::open(path)?;
    let mut lines: Vec<String> = std::io::BufReader::new(f)
        .lines()
        .collect::<std::io::Result<_>>()?;
    let raw = std::fs::read(path)?;
    let torn = !raw.is_empty() && raw.last() != Some(&b'\n');
    if torn {
        lines.pop();
    }
    let n = lines.len();
    let mut out = Vec::with_capacity(n);
    for (i, l) in lines.into_iter().enumerate() {
        if l.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Receipt>(&l) {
            Ok(r) => out.push(r),
            Err(_) if i + 1 == n => break,
            Err(e) => return Err(Error::corrupt(format!("receipt line {}: {e}", i + 1))),
        }
    }
    Ok(out)
}

/// Encode a path for a receipt (used by tests and the fixture).
pub fn path_fields(rel: &[u8]) -> (Option<String>, Option<String>) {
    enc_bytes(rel)
}
