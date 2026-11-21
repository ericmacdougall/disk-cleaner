//! Secret context by physical identity (docs/SCAN-FORMAT.md, "Context follows location").
//!
//! A directory or file is in secret context when a name says so wherever the walk meets it: a
//! secret-named component of its path, a spelling of a scan root, or a symlink whose own name is
//! secret or that lies in secret context. Names are attached to paths, but a symlink or a root
//! spelling names a directory or file by what it is, so the context it gives is keyed by the
//! physical identity (device, inode) of its target, and reaches that target however the walk
//! spelled it, in every root order.
//!
//! The union only ever adds context. It is built from the main walk and closed once, after the
//! walk and before any git process runs; the survey only reads it. On a platform without file
//! identity (device and inode 0) nothing is merged by identity: context then follows path names
//! only, as before, and every piece stays incomplete there anyway.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A symlink the walk saw.
#[derive(Debug, Clone)]
pub(crate) struct SeenLink {
    /// Where it is, as walked.
    pub path: PathBuf,
    /// The identity of what it names, symlinks followed (`None`: dangling, unreadable, or no
    /// identity on this platform).
    pub target: Option<(u64, u64)>,
    /// What it names is a directory.
    pub target_is_dir: bool,
    /// The secret kind its own name, or a secret-named component of its walked path, gives.
    pub named_kind: Option<String>,
}

/// The secret context the union reached: per identity, the kind of secret that reached it.
#[derive(Debug, Clone, Default)]
pub struct SecretContext {
    ids: HashMap<(u64, u64), String>,
    /// More symlinks with secret names than the walk records: the union may be missing context,
    /// so every piece of the scan is kept (incomplete).
    pub(crate) overflow: bool,
}

/// No identity on this platform or file system.
const NO_IDENTITY: (u64, u64) = (0, 0);

/// What [`SecretContext::close`] calls to list the symlinks below a directory that gained
/// context, when the walk recorded fewer symlinks than it saw.
pub(crate) type Relist<'a> = &'a mut dyn FnMut(&Path) -> Vec<SeenLink>;

impl SecretContext {
    /// Give `id` the secret kind `kind`; the first kind that reached it stays. Without identity
    /// nothing is merged. Returns whether it was new.
    pub fn add(&mut self, id: Option<(u64, u64)>, kind: &str) -> bool {
        match id {
            Some(id) if id != NO_IDENTITY && !self.ids.contains_key(&id) => {
                self.ids.insert(id, kind.to_string());
                true
            }
            _ => false,
        }
    }

    /// The secret kind that reached `id`, if any.
    pub fn kind_of(&self, id: (u64, u64)) -> Option<&str> {
        if id == NO_IDENTITY {
            return None;
        }
        self.ids.get(&id).map(String::as_str)
    }

    /// The identities in secret context.
    pub(crate) fn ids(&self) -> impl Iterator<Item = &(u64, u64)> {
        self.ids.keys()
    }

    /// Close the union over `links`: a link whose own name or walked path is secret gives what it
    /// names that context, and so does a link that lies below a directory in secret context; a
    /// directory that gains context passes it on to the links below it. `dir_paths` holds the
    /// walked paths of each directory identity that may gain context. `relist`, when the walk did
    /// not record every symlink, lists the links below a directory that gained context. Each
    /// identity gains context once, so this ends.
    pub(crate) fn close(
        &mut self,
        links: &mut [SeenLink],
        dir_paths: &HashMap<(u64, u64), Vec<PathBuf>>,
        mut relist: Option<Relist>,
    ) {
        links.sort_by(|a, b| a.path.cmp(&b.path));
        let mut work: Vec<(PathBuf, String)> = Vec::new();
        // Directories already in context (root spellings) pass it on.
        let seeded: Vec<((u64, u64), String)> =
            self.ids.iter().map(|(id, k)| (*id, k.clone())).collect();
        for (id, kind) in seeded {
            for p in dir_paths.get(&id).into_iter().flatten() {
                work.push((p.clone(), kind.clone()));
            }
        }
        for l in links.iter() {
            if let Some(k) = &l.named_kind {
                self.give(&mut work, l, k, dir_paths);
            }
        }
        while let Some((dir, kind)) = work.pop() {
            // Links are sorted by path, component by component, so those below `dir` follow it.
            let start = links.partition_point(|l| l.path.as_path() < dir.as_path());
            for l in links[start..].iter() {
                if !l.path.starts_with(&dir) {
                    break;
                }
                if l.path != dir {
                    self.give(&mut work, l, &kind, dir_paths);
                }
            }
            if let Some(relist) = relist.as_mut() {
                for l in relist(&dir) {
                    self.give(&mut work, &l, &kind, dir_paths);
                }
            }
        }
    }

    fn give(
        &mut self,
        work: &mut Vec<(PathBuf, String)>,
        l: &SeenLink,
        kind: &str,
        dir_paths: &HashMap<(u64, u64), Vec<PathBuf>>,
    ) {
        if self.add(l.target, kind) && l.target_is_dir {
            if let Some(t) = l.target {
                for p in dir_paths.get(&t).into_iter().flatten() {
                    work.push((p.clone(), kind.to_string()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(path: &str, target: (u64, u64), dir: bool, named: Option<&str>) -> SeenLink {
        SeenLink {
            path: PathBuf::from(path),
            target: Some(target),
            target_is_dir: dir,
            named_kind: named.map(str::to_string),
        }
    }

    #[test]
    fn context_without_identity_merges_nothing() {
        let mut c = SecretContext::default();
        assert!(!c.add(Some((0, 0)), "keys"));
        assert!(!c.add(None, "keys"));
        assert_eq!(c.kind_of((0, 0)), None);
        assert_eq!(c.ids().count(), 0);
        // Directories without identity are never one context: a secret-named link to one and
        // an ordinary link below another reach nothing.
        let mut dirs: HashMap<(u64, u64), Vec<PathBuf>> = HashMap::new();
        dirs.insert((0, 0), vec![PathBuf::from("/r/a"), PathBuf::from("/r/b")]);
        let mut links = vec![
            link("/r/.aws", (0, 0), true, Some("cloud keys")),
            link("/r/a/x", (0, 0), false, None),
        ];
        c.close(&mut links, &dirs, None);
        assert_eq!(c.ids().count(), 0);
    }

    #[test]
    fn context_a_secret_link_and_its_chain_reach_their_targets() {
        let mut c = SecretContext::default();
        let mut dirs: HashMap<(u64, u64), Vec<PathBuf>> = HashMap::new();
        dirs.insert((1, 10), vec![PathBuf::from("/r/vault/aws")]);
        dirs.insert((1, 20), vec![PathBuf::from("/r/far")]);
        let mut links = vec![
            // `.aws` names /r/vault/aws; a plain link inside it names /r/far, and one inside that
            // names a file; a plain link elsewhere gives nothing, nor does a sibling whose path
            // only starts with the same letters.
            link("/r/home/.aws", (1, 10), true, Some("cloud keys")),
            link("/r/vault/aws/more", (1, 20), true, None),
            link("/r/far/key", (1, 40), false, None),
            link("/r/plain/link", (1, 30), false, None),
            link("/r/vault/aws-old/x", (1, 50), false, None),
        ];
        c.close(&mut links, &dirs, None);
        assert_eq!(c.kind_of((1, 10)), Some("cloud keys"));
        assert_eq!(c.kind_of((1, 20)), Some("cloud keys"));
        assert_eq!(c.kind_of((1, 40)), Some("cloud keys"));
        assert_eq!(c.kind_of((1, 30)), None);
        assert_eq!(c.kind_of((1, 50)), None);
        assert_eq!(c.ids().count(), 3);
    }

    #[test]
    fn context_a_root_spelling_passes_its_context_to_links_below() {
        let mut c = SecretContext::default();
        let mut dirs: HashMap<(u64, u64), Vec<PathBuf>> = HashMap::new();
        dirs.insert((2, 1), vec![PathBuf::from("/plain")]);
        assert!(c.add(Some((2, 1)), "secret directory"));
        let mut links = vec![link("/plain/k", (2, 5), false, None)];
        c.close(&mut links, &dirs, None);
        assert_eq!(c.kind_of((2, 5)), Some("secret directory"));
    }

    #[test]
    fn context_a_cycle_ends() {
        let mut c = SecretContext::default();
        let mut dirs: HashMap<(u64, u64), Vec<PathBuf>> = HashMap::new();
        dirs.insert((3, 1), vec![PathBuf::from("/r/a")]);
        dirs.insert((3, 2), vec![PathBuf::from("/r/b")]);
        let mut links = vec![
            link("/r/.ssh", (3, 1), true, Some("ssh keys")),
            link("/r/a/to-b", (3, 2), true, None),
            link("/r/b/to-a", (3, 1), true, None),
        ];
        c.close(&mut links, &dirs, None);
        assert_eq!(c.ids().count(), 2);
    }

    #[test]
    fn context_relists_when_the_walk_did_not_record_every_link() {
        let mut c = SecretContext::default();
        let mut dirs: HashMap<(u64, u64), Vec<PathBuf>> = HashMap::new();
        dirs.insert((4, 1), vec![PathBuf::from("/r/vault")]);
        // Only the secret-named link was recorded; the one below the vault was not.
        let mut links = vec![link("/r/.aws", (4, 1), true, Some("cloud keys"))];
        let mut asked: Vec<PathBuf> = Vec::new();
        let mut relist = |dir: &Path| {
            asked.push(dir.to_path_buf());
            if dir == Path::new("/r/vault") {
                vec![link("/r/vault/key", (4, 9), false, None)]
            } else {
                Vec::new()
            }
        };
        c.close(&mut links, &dirs, Some(&mut relist));
        assert_eq!(c.kind_of((4, 9)), Some("cloud keys"));
        assert_eq!(asked, vec![PathBuf::from("/r/vault")]);
    }
}
