//! The apply journal: `<state dir>/journal/<plan id>.jsonl`, append-only, one fsynced record
//! per line, holding paths, sizes, rule ids, digests, (dev, ino) pairs and reasons only, never
//! file contents.
//!
//! **One owner of state.** This module is the only place that appends a record and the only
//! place that can make a [`Proof`]. Every mutating file operation in [`super::fsops`] takes a
//! `&Proof<K>` of the kind it performs, and a proof exists only after the matching intent
//! record has been written *and fsynced*. A mutation without an intent therefore does not
//! compile (the `compile_fail` example on [`Proof`] and the lint in `tests`), and the item
//! machine cannot invent a record shape because [`Journal`] offers one typed method per record.

use super::fsops::{Dir, FileId};
use super::{ApplyError, Result};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

/// Why an item was skipped (before any change, or because the rename failed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// The fresh plan does not agree with the reviewed one on this item.
    PlanChanged,
    /// Protected root recorded in the plan or resolved now (class E).
    Protected,
    /// The path is gone, is not a directory, or is a symlink.
    NotADirectory,
    /// A mount point lies at or below the item.
    Mountpoint,
    /// A process could reach the item (class D).
    InUseUnknown,
    /// The no-replace rename failed (target exists, another device, no filesystem support).
    Rename,
    /// Not walkable by directory fds (another device, a protected root's inode, unreadable).
    Walk,
}

/// A record of the journal, as written. `idx` is the index of the item in the plan file, which
/// the plan id (a hash of the file) pins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "rec", rename_all = "kebab-case")]
pub enum Record {
    Start {
        tool_version: String,
        /// `new` or `resume`.
        mode: String,
        candidates: u64,
    },
    Skip {
        idx: usize,
        path: String,
        reason: SkipReason,
        detail: String,
    },
    Quarantining {
        idx: usize,
        from: String,
        to: String,
        parent_dev: u64,
        parent_ino: u64,
        rule_id: Option<String>,
    },
    Quarantined {
        idx: usize,
        root_dev: u64,
        root_ino: u64,
        digest: String,
        entries: u64,
        bytes: u64,
    },
    Restoring {
        idx: usize,
        why: String,
    },
    RestoredInPlace {
        idx: usize,
        note: String,
    },
    KeptInQuarantine {
        idx: usize,
        why: String,
    },
    Removing {
        idx: usize,
    },
    Removed {
        idx: usize,
        bytes: u64,
    },
    End {
        mode: String,
        removed: u64,
        bytes: u64,
        skipped: u64,
        restored: u64,
        kept: u64,
    },
}

/// One line: a sequence number and a time ahead of the record's own fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    pub t: i64,
    #[serde(flatten)]
    pub rec: Record,
}

/// The kinds of mutation an intent can authorise.
pub trait Kind: sealed::Sealed {}
/// Rename an item into its quarantine name.
pub struct Quarantine;
/// Rename an item back to its original name.
pub struct Restore;
/// Delete the quarantined tree.
pub struct Remove;
impl Kind for Quarantine {}
impl Kind for Restore {}
impl Kind for Remove {}
mod sealed {
    pub trait Sealed {}
    impl Sealed for super::Quarantine {}
    impl Sealed for super::Restore {}
    impl Sealed for super::Remove {}
}

/// Proof that the intent record for one kind of mutation is on disk. Only [`Journal`] makes
/// one, and only after the fsync, so a mutation function that asks for it cannot run first.
///
/// ```compile_fail
/// // A proof cannot be built outside the journal module: the fields are private.
/// let _p: disk_cleaner_core::apply::journal::Proof<disk_cleaner_core::apply::journal::Remove> =
///     disk_cleaner_core::apply::journal::Proof { idx: 0, _kind: std::marker::PhantomData };
/// ```
pub struct Proof<K: Kind> {
    idx: usize,
    _kind: PhantomData<K>,
}

impl<K: Kind> Proof<K> {
    /// The plan item this intent was for.
    pub fn idx(&self) -> usize {
        self.idx
    }
}

/// An open journal for one plan id.
pub struct Journal {
    file: File,
    path: PathBuf,
    seq: u64,
    now: i64,
}

impl Journal {
    /// The journal directory of a state directory.
    pub fn dir_of(state_dir: &Path) -> PathBuf {
        state_dir.join("journal")
    }

    /// The journal file of a plan id.
    pub fn path_of(state_dir: &Path, plan_id: &str) -> PathBuf {
        Journal::dir_of(state_dir).join(format!("{plan_id}.jsonl"))
    }

    /// Open (creating it, its directory and the state directory when needed) the journal of
    /// `plan_id` for appending. The journal directory is fsynced after it is created.
    pub fn open(state_dir: &Path, plan_id: &str, now: i64) -> Result<Journal> {
        let dir = Journal::dir_of(state_dir);
        let existed = dir.is_dir();
        create_private_dirs(&dir)?;
        let path = Journal::path_of(state_dir, plan_id);
        let fresh = !path.exists();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| ApplyError::io(&path, e))?;
        if fresh || !existed {
            file.sync_all().map_err(|e| ApplyError::io(&path, e))?;
            sync_dir(&dir)?;
            if let Some(parent) = dir.parent() {
                sync_dir(parent)?;
            }
        }
        let seq = read_entries(&path)?.last().map(|e| e.seq + 1).unwrap_or(0);
        Ok(Journal {
            file,
            path,
            seq,
            now,
        })
    }

    /// The file this journal appends to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn append(&mut self, rec: Record) -> Result<()> {
        let entry = Entry {
            seq: self.seq,
            t: self.now,
            rec,
        };
        let mut line = serde_json::to_vec(&entry).map_err(ApplyError::Json)?;
        line.push(b'\n');
        // One write per record, so a kill leaves at most one torn last line, which readers drop.
        self.file
            .write_all(&line)
            .map_err(|e| ApplyError::io(&self.path, e))?;
        self.file
            .sync_all()
            .map_err(|e| ApplyError::io(&self.path, e))?;
        self.seq += 1;
        #[cfg(test)]
        super::hooks::journal_synced(&entry);
        Ok(())
    }

    fn proof<K: Kind>(idx: usize) -> Proof<K> {
        Proof {
            idx,
            _kind: PhantomData,
        }
    }

    // ---- facts: records that no mutation waits for ----

    pub fn start(&mut self, mode: &str, candidates: u64) -> Result<()> {
        self.append(Record::Start {
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            mode: mode.to_string(),
            candidates,
        })
    }

    pub fn skip(&mut self, idx: usize, path: &str, reason: SkipReason, detail: &str) -> Result<()> {
        self.append(Record::Skip {
            idx,
            path: path.to_string(),
            reason,
            detail: detail.to_string(),
        })
    }

    pub fn quarantined(
        &mut self,
        idx: usize,
        root: FileId,
        digest: &str,
        entries: u64,
        bytes: u64,
    ) -> Result<()> {
        self.append(Record::Quarantined {
            idx,
            root_dev: root.dev,
            root_ino: root.ino,
            digest: digest.to_string(),
            entries,
            bytes,
        })
    }

    pub fn restored_in_place(&mut self, idx: usize, note: &str) -> Result<()> {
        self.append(Record::RestoredInPlace {
            idx,
            note: note.to_string(),
        })
    }

    pub fn kept_in_quarantine(&mut self, idx: usize, why: &str) -> Result<()> {
        self.append(Record::KeptInQuarantine {
            idx,
            why: why.to_string(),
        })
    }

    pub fn removed(&mut self, idx: usize, bytes: u64) -> Result<()> {
        self.append(Record::Removed { idx, bytes })
    }

    pub fn end(&mut self, mode: &str, t: &Totals) -> Result<()> {
        self.append(Record::End {
            mode: mode.to_string(),
            removed: t.removed,
            bytes: t.bytes,
            skipped: t.skipped,
            restored: t.restored,
            kept: t.kept,
        })
    }

    // ---- intents: the only way to get a proof ----

    /// Write and fsync `quarantining`, then hand out the proof for the rename. The parent's
    /// identity and the target name are recorded so that resume can verify them.
    pub fn intent_quarantine(
        &mut self,
        idx: usize,
        from: &str,
        to: &str,
        parent: &Dir,
        rule_id: Option<&str>,
    ) -> Result<Proof<Quarantine>> {
        self.append(Record::Quarantining {
            idx,
            from: from.to_string(),
            to: to.to_string(),
            parent_dev: parent.id().dev,
            parent_ino: parent.id().ino,
            rule_id: rule_id.map(str::to_string),
        })?;
        Ok(Journal::proof(idx))
    }

    /// Write and fsync `restoring`, then hand out the proof for the rename back.
    pub fn intent_restore(&mut self, idx: usize, why: &str) -> Result<Proof<Restore>> {
        self.append(Record::Restoring {
            idx,
            why: why.to_string(),
        })?;
        Ok(Journal::proof(idx))
    }

    /// Write and fsync `removing`, then hand out the proof for deleting the tree.
    pub fn intent_remove(&mut self, idx: usize) -> Result<Proof<Remove>> {
        self.append(Record::Removing { idx })?;
        Ok(Journal::proof(idx))
    }
}

/// Totals of a run, for the `end` record and the report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Totals {
    pub removed: u64,
    pub bytes: u64,
    pub skipped: u64,
    pub restored: u64,
    pub kept: u64,
}

fn create_private_dirs(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| ApplyError::io(dir, e))
}

/// fsync a directory.
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| ApplyError::io(dir, e))
}

/// Read a journal. A torn last line (the process was killed mid-write) is dropped: its intent was
/// never durable, so nothing followed it. A bad line anywhere else is an error.
pub fn read_entries(path: &Path) -> Result<Vec<Entry>> {
    let text = match std::fs::read(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ApplyError::io(path, e)),
    };
    let text = String::from_utf8_lossy(&text);
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(line) {
            Ok(e) => out.push(e),
            Err(_) if i + 1 == lines.len() => {} // no trailing newline: torn
            Err(e) => {
                return Err(ApplyError::Corrupt {
                    path: path.to_path_buf(),
                    line: i + 1,
                    message: e.to_string(),
                })
            }
        }
    }
    Ok(out)
}

/// Where an item stands according to the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Skipped,
    Quarantining,
    Quarantined,
    Restoring,
    Removing,
    Removed,
    RestoredInPlace,
    KeptInQuarantine,
}

impl State {
    /// Nothing left to reconcile.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            State::Skipped | State::Removed | State::RestoredInPlace | State::KeptInQuarantine
        )
    }
}

/// What the journal knows about one item, from its latest records.
#[derive(Debug, Clone)]
pub struct ItemLog {
    pub idx: usize,
    pub state: State,
    pub from: String,
    pub to: String,
    pub parent: FileId,
    pub root: Option<FileId>,
    pub digest: Option<String>,
    pub bytes: u64,
}

/// Fold a journal into the latest state per item. A later run of the same plan can start an
/// item again: its `quarantining` record replaces what was known.
pub fn fold(entries: &[Entry]) -> Vec<ItemLog> {
    let mut logs: Vec<ItemLog> = Vec::new();
    for e in entries {
        match &e.rec {
            Record::Skip { idx, .. } => match logs.iter().position(|l| l.idx == *idx) {
                // A skip also ends a `quarantining` whose rename failed.
                Some(k) => logs[k].state = State::Skipped,
                None => logs.push(ItemLog {
                    idx: *idx,
                    state: State::Skipped,
                    from: String::new(),
                    to: String::new(),
                    parent: FileId { dev: 0, ino: 0 },
                    root: None,
                    digest: None,
                    bytes: 0,
                }),
            },
            Record::Quarantining {
                idx,
                from,
                to,
                parent_dev,
                parent_ino,
                ..
            } => {
                let log = ItemLog {
                    idx: *idx,
                    state: State::Quarantining,
                    from: from.clone(),
                    to: to.clone(),
                    parent: FileId {
                        dev: *parent_dev,
                        ino: *parent_ino,
                    },
                    root: None,
                    digest: None,
                    bytes: 0,
                };
                match logs.iter().position(|l| l.idx == *idx) {
                    Some(k) => logs[k] = log,
                    None => logs.push(log),
                }
            }
            Record::Quarantined {
                idx,
                root_dev,
                root_ino,
                digest,
                bytes,
                ..
            } => {
                if let Some(l) = logs.iter_mut().find(|l| l.idx == *idx) {
                    l.state = State::Quarantined;
                    l.root = Some(FileId {
                        dev: *root_dev,
                        ino: *root_ino,
                    });
                    l.digest = Some(digest.clone());
                    l.bytes = *bytes;
                }
            }
            Record::Restoring { idx, .. } => set(&mut logs, *idx, State::Restoring),
            Record::RestoredInPlace { idx, .. } => set(&mut logs, *idx, State::RestoredInPlace),
            Record::KeptInQuarantine { idx, .. } => set(&mut logs, *idx, State::KeptInQuarantine),
            Record::Removing { idx } => set(&mut logs, *idx, State::Removing),
            Record::Removed { idx, .. } => set(&mut logs, *idx, State::Removed),
            Record::Start { .. } | Record::End { .. } => {}
        }
    }
    logs
}

fn set(logs: &mut [ItemLog], idx: usize, st: State) {
    if let Some(l) = logs.iter_mut().find(|l| l.idx == idx) {
        l.state = st;
    }
}

/// Items of this journal that are not in a final state.
pub fn unfinished(path: &Path) -> Result<Vec<ItemLog>> {
    Ok(fold(&read_entries(path)?)
        .into_iter()
        .filter(|l| !l.state.is_final())
        .collect())
}

/// Plan ids that have a journal in the state directory, with their unfinished items (read-only).
pub fn all_unfinished(state_dir: &Path) -> Vec<(String, Vec<ItemLog>)> {
    let dir = Journal::dir_of(state_dir);
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    let mut files: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    for p in files {
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Ok(items) = unfinished(&p) {
            if !items.is_empty() {
                out.push((id.to_string(), items));
            }
        }
    }
    out
}
