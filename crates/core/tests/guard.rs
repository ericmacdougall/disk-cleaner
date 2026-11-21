//! The generative guard of the lanes classifier (GoodQA goodqa#16529, AC-8).
//!
//! It composes a few hundred small layouts from fixed seeds (printed), scans each one with
//! lanes, plans from the scan file read back, and checks what was assigned and what was read
//! against what the generator knows it built. The oracle never asks the product: it is the set
//! of file identities (device, inode) the generator created in each place, so an alias
//! spelling, a lossy name or a hard link cannot hide a member from it. The invariants
//! (docs/SCAN-FORMAT.md, "Pieces and lanes", and the rescue spec):
//!
//! - a (AC-1, AC-2, AC-3, AC-5, AC-19): nothing secret, nothing inside a git directory other
//!   than its own repository's `git-node` piece, no work tree of a repository the scan does not
//!   own, no pinned target and no held repository is in a removing lane, and a member in a
//!   secret place that names no repository is a secret piece.
//! - b (AC-1, AC-5): no file in secret context is opened (the read recorder at the content gate)
//!   or hashed (blobs), and no git process runs for a repository in a secret place (a recording
//!   wrapper given to `Git::new`, as rescue.rs n6 does).
//! - c (AC-16): no emitted path (roots, node paths, item paths, scan and plan piece paths, blob
//!   copy paths) has a symlinked component, a `.` or `..` component, or lies outside the real
//!   paths of the roots. Every component is `lstat`ed.
//! - d (AC-13): the survey's entries and bytes plus `dir_overhead_bytes` equal the main walk's
//!   `files` and `allocated_bytes`.
//! - e (AC-21): every `keep` piece under the path of an item with a removing action is a secret
//!   piece.
//! - f (NEG-10): each layout's controls still reclaim: loose files backed up (or noise) and
//!   hashed, an artifact that is noise, and a pushed repository in the git lane for which git
//!   ran. The git lane is required only where nothing in the layout can make that repository's
//!   facts unknown (no `.git` file and no git-data reference that leaves the scan); git must run
//!   for it in every layout.
//! - g (docs, "a path is exact"): an entry whose name is not valid UTF-8 is kept and is never
//!   opened or hashed.
//!
//! Every plan is made by `idle_plan` from the scan file read back (AC-11). Every violation is
//! collected with its layout, seed and dimensions before the test fails, and a layout that
//! errors, panics or is skipped fails the test. `GUARD_ONLY=3,17` runs only those layouts (to
//! reproduce one) and then always fails, as a partial run.
//!
//! Unix only: the layouts use symbolic links and hard links.

#![cfg(unix)]

mod common;

use common::*;
use disk_cleaner_core::pieces::PieceOptions;
use disk_cleaner_core::{
    plan, scan, scan_from_json, testing, Git, Lane, LanePiece, PieceKind, Plan, PlanOptions,
    ScanOptions, ScanResult,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const KIB: usize = 1024;
/// The fixed seeds: layout `i` takes its seed from `SEEDS[i / PER_SEED]` and `i`.
const SEEDS: [u64; 2] = [0x6a09_e667_f3bc_c908, 0xbb67_ae85_84ca_a73b];
const PER_SEED: usize = 112;
const MIN_LAYOUTS: usize = 200;
const MIN_PER_DIMENSION: usize = 5;
const MIN_PER_VARIANT: usize = 2;
const MAX_EXAMPLES: usize = 40;
const BUILD_THREADS: usize = 8;
const SCAN_THREADS: usize = 6;

// ----------------------------------------------------------------------------------------------
// A tiny deterministic generator (splitmix64), so no new crate is needed and every run of a seed
// builds the same layouts.

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

fn layout_seed(i: usize) -> u64 {
    let mut r = Rng(SEEDS[i / PER_SEED] ^ (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
    r.next()
}

// ----------------------------------------------------------------------------------------------
// Dimensions and their variants. Layout `i` has the primary feature `(DIMS[i % 7], (i / 7) %
// variants)`, so every variant is built; up to two secondary features are drawn from the seed.

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Dim {
    RootOrder,
    AliasRoots,
    GitShape,
    DepEdge,
    AboveRoot,
    OddNames,
    SecretPlace,
}

const DIMS: [Dim; 7] = [
    Dim::RootOrder,
    Dim::AliasRoots,
    Dim::GitShape,
    Dim::DepEdge,
    Dim::AboveRoot,
    Dim::OddNames,
    Dim::SecretPlace,
];

impl Dim {
    fn name(self) -> &'static str {
        match self {
            Dim::RootOrder => "roots-in-every-order",
            Dim::AliasRoots => "alias-and-duplicate-roots",
            Dim::GitShape => "git-directory-shapes",
            Dim::DepEdge => "dependency-edges",
            Dim::AboveRoot => "symlinks-above-a-root",
            Dim::OddNames => "invalid-and-case-variant-names",
            Dim::SecretPlace => "secret-places",
        }
    }

    fn variants(self) -> &'static [&'static str] {
        match self {
            Dim::RootOrder => &[
                "plain-012",
                "plain-021",
                "plain-102",
                "plain-120",
                "plain-201",
                "plain-210",
                "secret-012",
                "secret-021",
                "secret-102",
                "secret-120",
                "secret-201",
                "secret-210",
            ],
            Dim::AliasRoots => &[
                "link-duplicate",
                "secret-alias",
                "secret-alias-nested",
                "same-root-twice",
                "dotdot",
                "dotdot-through-link",
                "secret-alias-dotdot",
            ],
            Dim::GitShape => &[
                "dot-git-dir",
                "dot-git-file",
                "bare",
                "head-commondir",
                "head-symlink",
                "upper-dot-git",
                "linked-worktree",
                "cache-checkout",
                "node-modules-checkout",
            ],
            Dim::DepEdge => &[
                "alternates-in",
                "alternates-out",
                "commondir-in",
                "commondir-out",
                "git-file-in",
                "git-file-out",
                "dir-link-in",
                "dir-link-out",
                "file-link-in",
                "file-link-out",
                "core-worktree-in",
                "core-worktree-out",
                "include-in",
                "include-out",
                "include-if-in",
                "include-if-out",
            ],
            Dim::AboveRoot => &["link", "secret-link", "secret-real", "hops", "link-repo"],
            Dim::OddNames => &[
                "bad-utf8-file",
                "bad-utf8-dir",
                "bad-utf8-secret",
                "case-env",
                "case-ssh",
                "case-secrets",
                "case-key",
                "case-git",
            ],
            Dim::SecretPlace => &[
                "ssh-dir",
                "password-store-repo",
                "secrets-repo",
                "secret-named-links",
                "link-in-secret-dir",
                "aws-link-to-parent",
                "repo-through-aws-root",
                "hard-linked-env",
                "secret-entry-in-artifact",
            ],
        }
    }
}

const PERMS: [[usize; 3]; 6] = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
];

// ----------------------------------------------------------------------------------------------
// The oracle: places the generator built, each with the identities of every entry below it.

type Id = (u64, u64);

fn id_of(md: &fs::Metadata) -> Id {
    (md.dev(), md.ino())
}

/// Identity of the entry itself (a final symlink is not followed).
fn lid(p: &Path) -> Option<Id> {
    fs::symlink_metadata(p).ok().map(|m| id_of(&m))
}

/// Identity of what the path names (symlinks followed).
fn fid(p: &Path) -> Option<Id> {
    fs::metadata(p).ok().map(|m| id_of(&m))
}

#[derive(Default, Clone)]
struct Ids {
    files: HashSet<Id>,
    dirs: HashSet<Id>,
}

/// Every entry at and below `p`, never following a symlink.
fn ids_under(p: &Path) -> Ids {
    let mut out = Ids::default();
    let mut stack = vec![p.to_path_buf()];
    while let Some(q) = stack.pop() {
        let md = fs::symlink_metadata(&q)
            .unwrap_or_else(|e| panic!("generator: lstat {}: {e}", q.display()));
        if md.is_dir() {
            out.dirs.insert(id_of(&md));
            for e in fs::read_dir(&q).unwrap() {
                stack.push(e.unwrap().path());
            }
        } else {
            out.files.insert(id_of(&md));
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum What {
    /// Secret context that holds a repository: kept, never opened or hashed.
    Secret,
    /// Secret context: kept as secret pieces, never opened or hashed.
    SecretFlag,
    /// A git directory: removed only as part of `owner`'s own `git-node` piece (none: never).
    GitDir,
    /// The work tree of a repository the scan does not own.
    Foreign,
    /// What a git-data reference points to.
    Pinned,
    /// A repository the spec holds (a `core.worktree` repository).
    Kept,
    /// A name that is not valid UTF-8.
    Inexact,
}

impl What {
    fn invariant(self) -> &'static str {
        match self {
            What::Secret | What::SecretFlag => "a:secret",
            What::GitDir => "a:git-dir",
            What::Foreign => "a:foreign-work-tree",
            What::Pinned => "a:pinned",
            What::Kept => "a:held-repository",
            What::Inexact => "g:inexact",
        }
    }
}

struct Place {
    what: What,
    label: String,
    owner: Option<Id>,
    ids: Ids,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ctl {
    Loose,
    Noise,
    Git,
}

struct Control {
    kind: Ctl,
    label: String,
    ids: Ids,
}

struct Layout {
    index: usize,
    seed: u64,
    dims: BTreeSet<Dim>,
    tags: Vec<String>,
    _tmp: tempfile::TempDir,
    base: PathBuf,
    roots: Vec<PathBuf>,
    real_roots: Vec<PathBuf>,
    po: PieceOptions,
    wrapper: PathBuf,
    log: PathBuf,
    places: Vec<Place>,
    /// Repositories in a secret place: no git process may run in or for them.
    no_git: Vec<(String, Ids)>,
    controls: Vec<Control>,
    git_live: bool,
}

// ----------------------------------------------------------------------------------------------
// Fixture pieces.

/// Two byte-identical files big enough to be hashed, plus a small one: every protected place
/// holds them, so "not opened, not hashed" is never vacuous.
fn content(dir: &Path) {
    write_bytes(&dir.join("x.bin"), 100 * KIB);
    write_bytes(&dir.join("y.bin"), 100 * KIB);
    write(&dir.join("notes.txt"), "notes\n");
}

fn s(p: &Path) -> &str {
    p.to_str().expect("fixture paths are UTF-8")
}

/// A git directory git accepts by HEAD and commondir under an ordinary name (rescue a3f2).
fn borrower(dir: &Path, provider_git: &Path, head_symlink: bool) {
    fs::create_dir_all(dir).unwrap();
    if head_symlink {
        symlink("refs/heads/main", dir.join("HEAD")).unwrap();
    } else {
        write(&dir.join("HEAD"), "ref: refs/heads/main\n");
    }
    write(
        &dir.join("commondir"),
        &format!("{}\n", provider_git.display()),
    );
    write_bytes(&dir.join("index"), 70 * KIB);
    let out = git_command()
        .arg("--git-dir")
        .arg(dir)
        .args(["rev-parse", "--git-dir"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "fixture: git accepts {} as a git directory: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository whose git directory is `store` (`dir/.git` is a file naming it).
fn separate_repo(dir: &Path, store: &Path) {
    let parent = dir.parent().unwrap();
    fs::create_dir_all(parent).unwrap();
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    git(
        parent,
        &[
            "init",
            "-q",
            "-b",
            "main",
            "--separate-git-dir",
            s(store),
            s(dir),
        ],
    );
    git(dir, &["config", "commit.gpgsign", "false"]);
    write(&dir.join("README.md"), "hello\n");
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

fn init_bare_at(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "--bare", "-b", "main"]);
}

fn append(path: &Path, text: &str) {
    let mut old = fs::read_to_string(path).unwrap();
    old.push_str(text);
    fs::write(path, old).unwrap();
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Placement {
    Below,
    OwnRoot,
    AliasRoot,
    SecondRoot,
}

/// The layout under construction.
struct B {
    rng: Rng,
    base: PathBuf,
    w: PathBuf,
    out: PathBuf,
    remotes: PathBuf,
    r0: PathBuf,
    roots: Vec<PathBuf>,
    triple: Option<(Vec<PathBuf>, [usize; 3])>,
    places: Vec<(What, PathBuf, Option<PathBuf>)>,
    no_git: Vec<PathBuf>,
    controls: Vec<(Ctl, PathBuf, Vec<PathBuf>)>,
    git_live: bool,
    /// Pieces of at most 64 KiB, so a directory of two 100 KiB files is split.
    small: bool,
    dims: BTreeSet<Dim>,
    tags: Vec<String>,
    n: usize,
}

impl B {
    fn new(base: PathBuf, seed: u64) -> B {
        let w = base.join("w");
        let out = base.join("outside");
        let remotes = base.join("remotes");
        let r0 = w.join("r0");
        for d in [&w, &out, &remotes, &r0] {
            fs::create_dir_all(d).unwrap();
        }
        let mut rng = Rng(seed);
        let small = rng.chance(30);
        let mut b = B {
            rng,
            small,
            base,
            w,
            out,
            remotes,
            r0,
            roots: Vec::new(),
            triple: None,
            places: Vec::new(),
            no_git: Vec::new(),
            controls: Vec::new(),
            git_live: true,
            dims: BTreeSet::new(),
            tags: Vec::new(),
            n: 0,
        };
        // The controls, one per removing lane (NEG-10), in a root of their own.
        let ctl = b.w.join("ctl");
        content(&ctl.join("loose"));
        b.control(Ctl::Loose, &ctl.join("loose"), &[]);
        let gitctl = ctl.join("gitctl");
        b.pushed_repo(&gitctl);
        b.control(Ctl::Git, &gitctl, &[]);
        let js = ctl.join("js");
        write(&js.join("package.json"), "{}\n");
        let pkg = js.join("node_modules").join("pkg");
        write(&pkg.join("package.json"), "{\"name\": \"pkg\"}\n");
        write_bytes(&pkg.join("index.js"), 9 * KIB);
        b.control(Ctl::Noise, &js.join("node_modules"), &[]);
        b.roots.push(b.r0.clone());
        b.roots.push(ctl);
        b
    }

    fn fresh(&mut self, stem: &str) -> String {
        self.n += 1;
        format!("{stem}{}", self.n)
    }

    fn wdir(&mut self, stem: &str) -> PathBuf {
        let n = self.fresh(stem);
        self.w.join(n)
    }

    fn r0dir(&mut self, stem: &str) -> PathBuf {
        let n = self.fresh(stem);
        self.r0.join(n)
    }

    fn outdir(&mut self, stem: &str) -> PathBuf {
        let n = self.fresh(stem);
        self.out.join(n)
    }

    fn basedir(&mut self, stem: &str) -> PathBuf {
        let n = self.fresh(stem);
        let d = self.base.join(n);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn tag(&mut self, t: String) {
        self.tags.push(t);
    }

    fn mark(&mut self, what: What, path: &Path, owner: Option<&Path>) {
        self.places
            .push((what, path.to_path_buf(), owner.map(Path::to_path_buf)));
    }

    fn control(&mut self, kind: Ctl, path: &Path, exclude: &[PathBuf]) {
        self.controls
            .push((kind, path.to_path_buf(), exclude.to_vec()));
    }

    /// An ordinary repository the scan owns: its `.git` is removed only with its own piece.
    fn plain_repo(&mut self, dir: &Path) {
        init_repo(dir);
        self.mark(What::GitDir, &dir.join(".git"), Some(dir));
    }

    fn pushed_repo(&mut self, dir: &Path) {
        init_repo(dir);
        let name = self.fresh("remote");
        let remote = self.remotes.join(format!("{name}.git"));
        init_bare(&remote);
        push_to_remote(dir, &remote);
        self.mark(What::GitDir, &dir.join(".git"), Some(dir));
    }

    /// A repository in a secret place (with content): kept, no git, nothing read.
    fn secret_repo(&mut self, dir: &Path) {
        init_repo(dir);
        content(&dir.join("d"));
        self.mark(What::Secret, dir, None);
        self.no_git.push(dir.to_path_buf());
    }

    /// A symlink with a secret directory name, in a directory of its own, naming `target`.
    fn secret_alias(&mut self, target: &Path) -> PathBuf {
        let d = self.wdir("ap");
        fs::create_dir_all(&d).unwrap();
        let name = [".aws", "secrets", ".ssh"][self.rng.below(3)];
        let a = d.join(name);
        symlink(target, &a).unwrap();
        self.tag(format!("alias-name={name}"));
        a
    }

    fn placement(&mut self, own_ok: bool) -> (PathBuf, Placement) {
        let choices: &[Placement] = if own_ok {
            &[
                Placement::Below,
                Placement::OwnRoot,
                Placement::AliasRoot,
                Placement::SecondRoot,
            ]
        } else {
            &[Placement::Below, Placement::SecondRoot]
        };
        let pl = choices[self.rng.below(choices.len())];
        self.tag(format!("placement={pl:?}"));
        let x = match pl {
            Placement::Below => self.r0.clone(),
            Placement::SecondRoot => {
                let d = self.wdir("r");
                fs::create_dir_all(&d).unwrap();
                self.roots.push(d.clone());
                d
            }
            Placement::OwnRoot | Placement::AliasRoot => {
                let d = self.wdir("g");
                fs::create_dir_all(&d).unwrap();
                d
            }
        };
        (x, pl)
    }

    /// Give `dir` its own root for an own-root or alias-root placement.
    fn root_for(&mut self, dir: &Path, pl: Placement) {
        match pl {
            Placement::OwnRoot => self.roots.push(dir.to_path_buf()),
            Placement::AliasRoot => {
                let a = self.wdir("al");
                symlink(dir, &a).unwrap();
                self.roots.push(a);
            }
            Placement::Below | Placement::SecondRoot => {}
        }
    }

    fn feature(&mut self, dim: Dim, v: usize) {
        let name = dim.variants()[v];
        self.dims.insert(dim);
        match dim {
            Dim::RootOrder => self.root_order(v),
            Dim::AliasRoots => self.alias_roots(name),
            Dim::GitShape => self.git_shape(name),
            Dim::DepEdge => self.dep_edge(name),
            Dim::AboveRoot => self.above_root(name),
            Dim::OddNames => self.odd_names(name),
            Dim::SecretPlace => self.secret_place(name),
        }
    }

    // ------------------------------------------------------------------------------------------
    // Roots in every order: a parent, a nested root and an alias of the nested root (ordinary or
    // secret-named), in each of the six relative orders, among the layout's other roots.

    fn root_order(&mut self, v: usize) {
        let perm = PERMS[v % 6];
        let secret = v >= 6;
        let p = self.wdir("p");
        let kid = p.join("kid");
        content(&kid.join("data"));
        let gnupg = kid.join(".gnupg");
        content(&gnupg);
        write(&gnupg.join("pubring.kbx"), "not a keyring\n");
        content(&p.join("other"));
        self.mark(What::SecretFlag, &gnupg, None);
        let alias = if secret {
            self.secret_alias(&kid)
        } else {
            let d = self.wdir("ap");
            fs::create_dir_all(&d).unwrap();
            let a = d.join("kid-link");
            symlink(&kid, &a).unwrap();
            a
        };
        if secret {
            self.mark(What::SecretFlag, &kid.join("data"), None);
            self.secret_repo(&kid.join("r"));
        } else {
            self.plain_repo(&kid.join("r"));
            self.control(Ctl::Loose, &kid.join("data"), &[]);
        }
        self.control(Ctl::Loose, &p.join("other"), &[]);
        self.triple = Some((vec![p, kid, alias], perm));
    }

    // ------------------------------------------------------------------------------------------
    // Alias and duplicate roots.

    fn alias_roots(&mut self, name: &str) {
        let plain = self.wdir("plain");
        match name {
            "link-duplicate" => {
                content(&plain.join("d"));
                self.plain_repo(&plain.join("r"));
                let l = self.wdir("lnk");
                symlink(&plain, &l).unwrap();
                self.roots.push(plain.clone());
                self.roots.push(l);
                self.control(Ctl::Loose, &plain.join("d"), &[]);
            }
            "secret-alias" => {
                content(&plain.join("d"));
                self.secret_repo(&plain.join("r"));
                let a = self.secret_alias(&plain);
                self.roots.push(plain.clone());
                self.roots.push(a);
                self.mark(What::SecretFlag, &plain.join("d"), None);
            }
            "secret-alias-nested" => {
                content(&plain.join("sub"));
                content(&plain.join("other"));
                let a = self.secret_alias(&plain.join("sub"));
                self.roots.push(plain.clone());
                self.roots.push(a);
                self.mark(What::SecretFlag, &plain.join("sub"), None);
                // NEG-1: the rest of the root is still backed up and opened.
                self.control(Ctl::Loose, &plain.join("other"), &[]);
            }
            "same-root-twice" => {
                content(&plain.join("d"));
                self.plain_repo(&plain.join("r"));
                self.roots.push(plain.clone());
                self.roots.push(plain.clone());
                self.control(Ctl::Loose, &plain.join("d"), &[]);
            }
            "dotdot" => {
                content(&plain.join("d"));
                fs::create_dir_all(plain.join("sub")).unwrap();
                self.roots.push(plain.join("sub").join(".."));
                self.roots.push(plain.clone());
                self.control(Ctl::Loose, &plain.join("d"), &[]);
            }
            "dotdot-through-link" => {
                // `lnk/..` is the parent of the link's target, never the link's own parent.
                let deep = self.wdir("deep");
                content(&deep.join("y"));
                fs::create_dir_all(deep.join("x")).unwrap();
                fs::create_dir_all(&plain).unwrap();
                let l = self.wdir("lnk");
                symlink(deep.join("x"), &l).unwrap();
                self.roots.push(l.join(".."));
                self.control(Ctl::Loose, &deep.join("y"), &[]);
            }
            "secret-alias-dotdot" => {
                // A secret-named component of a root's spelling names `plain/sub` (AC-1).
                content(&plain.join("sub"));
                content(&plain.join("other"));
                let a = self.secret_alias(&plain.join("sub"));
                self.roots.push(a.join(".."));
                self.mark(What::SecretFlag, &plain.join("sub"), None);
            }
            other => unreachable!("{other}"),
        }
    }

    // ------------------------------------------------------------------------------------------
    // Every git-directory shape, below a root, as its own root, through an alias root, or below
    // a second root.

    fn git_shape(&mut self, name: &str) {
        let name = if cfg!(target_os = "linux") || name != "upper-dot-git" {
            name
        } else {
            // A case-insensitive file system makes `.GIT` the repository's own `.git`.
            "dot-git-dir"
        };
        self.tag(format!("shape={name}"));
        match name {
            "dot-git-dir" => {
                let (x, pl) = self.placement(true);
                let n = self.fresh("repo");
                let dir = x.join(n);
                self.pushed_repo(&dir);
                content(&dir.join("work"));
                self.root_for(&dir, pl);
            }
            "dot-git-file" => {
                let (x, _) = self.placement(false);
                let n = self.fresh("sep");
                let dir = x.join(n);
                let inside = self.rng.chance(50);
                let store = if inside {
                    self.r0dir("store").join("r.git")
                } else {
                    self.outdir("store").join("r.git")
                };
                separate_repo(&dir, &store);
                self.tag(format!("store-inside={inside}"));
                self.mark(What::GitDir, &dir.join(".git"), Some(&dir));
                if inside {
                    self.mark(What::GitDir, &store, None);
                    self.mark(What::Pinned, &store, None);
                }
                self.git_live = false;
            }
            "bare" => {
                let (x, pl) = self.placement(true);
                let n = self.fresh("bare");
                let dir = x.join(format!("{n}.git"));
                init_bare_at(&dir);
                self.mark(What::GitDir, &dir, Some(&dir));
                self.root_for(&dir, pl);
            }
            "head-commondir" | "head-symlink" => {
                let prov = self.r0dir("prov");
                self.pushed_repo(&prov);
                self.mark(What::Pinned, &prov.join(".git"), None);
                let (x, pl) = self.placement(true);
                let n = self.fresh("m");
                let m = x.join("work").join(n);
                borrower(&m, &prov.join(".git"), name == "head-symlink");
                self.mark(What::GitDir, &m, None);
                self.root_for(&m, pl);
            }
            "upper-dot-git" => {
                let (x, _) = self.placement(false);
                let n = self.fresh("up");
                let wt = x.join(n);
                init_bare_at(&wt.join(".GIT"));
                content(&wt);
                self.mark(What::GitDir, &wt.join(".GIT"), None);
                self.mark(What::Foreign, &wt, None);
            }
            "linked-worktree" => {
                let (x, _) = self.placement(false);
                let inside = self.rng.chance(50);
                let main = if inside {
                    self.r0dir("main")
                } else {
                    self.outdir("main")
                };
                init_repo(&main);
                let n = self.fresh("wt");
                let wt = x.join(&n);
                fs::create_dir_all(&x).unwrap();
                git(&main, &["worktree", "add", "-q", "-b", &n, s(&wt)]);
                self.tag(format!("main-inside={inside}"));
                self.mark(What::GitDir, &wt.join(".git"), Some(&wt));
                if inside {
                    self.mark(What::GitDir, &main.join(".git"), Some(&main));
                    // The worktree's `.git` file and its admin directory's commondir name the
                    // main repository's git directory (AC-3).
                    self.mark(What::Pinned, &main.join(".git"), None);
                }
                self.git_live = false;
            }
            "cache-checkout" => {
                let (x, _) = self.placement(false);
                let n = self.fresh("dep");
                let co = x
                    .join(".cargo")
                    .join("git")
                    .join("checkouts")
                    .join(format!("{n}-0123456789abcdef"))
                    .join("abc1234");
                init_repo(&co);
                content(&co.join("src"));
                write_bytes(
                    &x.join(".cargo").join("git").join("db").join("p.pack"),
                    9 * KIB,
                );
                self.mark(What::Foreign, &co, None);
                self.mark(What::GitDir, &co.join(".git"), None);
            }
            "node-modules-checkout" => {
                let (x, _) = self.placement(false);
                let n = self.fresh("proj");
                let proj = x.join(n);
                write(&proj.join("package.json"), "{}\n");
                let dep = proj.join("node_modules").join("dep");
                init_repo(&dep);
                content(&dep);
                let sib = proj.join("node_modules").join("plain");
                content(&sib);
                self.mark(What::Foreign, &dep, None);
                self.mark(What::GitDir, &dep.join(".git"), None);
                // NEG-4: once node_modules is split into several pieces, the sibling package is
                // still removed. Unsplit, it shares a piece with the checkout's git directory,
                // and that piece is rightly kept whole.
                if self.small {
                    self.control(Ctl::Loose, &sib, &[]);
                }
            }
            other => unreachable!("{other}"),
        }
    }

    // ------------------------------------------------------------------------------------------
    // Dependency edges: every git-data reference kind, pointing inside or outside the scan.

    fn dep_edge(&mut self, name: &str) {
        let (kind, inside) = match name.strip_suffix("-in") {
            Some(k) => (k, true),
            None => (name.strip_suffix("-out").unwrap(), false),
        };
        if !inside {
            // A reference out of the scan pins nothing; the repositories' facts stay unknown.
            self.git_live = false;
        }
        match kind {
            "alternates" => {
                // An alternates entry that names no git directory of the scan leaves every
                // repository's no_dependents unknown, inside the scan too (the spec keeps
                // today's facts; it only adds the pin).
                self.git_live = false;
                let holder = self.r0dir("alt");
                self.pushed_repo(&holder);
                let target = if inside {
                    let t = match self.rng.below(5) {
                        0 => {
                            let js = self.r0dir("js");
                            write(&js.join("package.json"), "{}\n");
                            self.tag("holder=artifact".into());
                            js.join("node_modules").join("store")
                        }
                        1 => {
                            let js = self.r0dir("js");
                            write(&js.join("package.json"), "{}\n");
                            // A backup-answerable ask found first: the pin must still count.
                            let tf = js.join("node_modules").join("aaa").join("prod.tfstate");
                            write(&tf, "{}\n");
                            self.mark(What::SecretFlag, &tf, None);
                            self.tag("holder=artifact-state-file-first".into());
                            js.join("node_modules").join("store")
                        }
                        2 => {
                            let lib = self.r0dir("lib");
                            write(&lib.join("main.py"), "print(1)\n");
                            self.tag("holder=loose-code".into());
                            lib.join("store")
                        }
                        3 => {
                            self.tag("holder=no-node".into());
                            self.r0dir("plain").join("store")
                        }
                        _ => {
                            // NEG-17: into the repository's own node_modules.
                            write(&holder.join("package.json"), "{}\n");
                            self.tag("holder=own-node-modules".into());
                            holder.join("node_modules").join("store")
                        }
                    };
                    content(&t);
                    self.mark(What::Pinned, &t, None);
                    t
                } else if self.rng.chance(50) {
                    let t = self.outdir("store");
                    content(&t);
                    self.tag("target=outside".into());
                    t
                } else {
                    self.tag("target=dangling".into());
                    self.outdir("missing")
                };
                write(
                    &holder
                        .join(".git")
                        .join("objects")
                        .join("info")
                        .join("alternates"),
                    &format!("{}\n", target.display()),
                );
            }
            "commondir" => {
                let prov = if inside {
                    self.r0dir("prov")
                } else {
                    self.outdir("prov")
                };
                if inside {
                    self.pushed_repo(&prov);
                    self.mark(What::Pinned, &prov.join(".git"), None);
                } else {
                    init_repo(&prov);
                }
                let n = self.fresh("m");
                let m = self.r0.join("work").join(n);
                borrower(&m, &prov.join(".git"), self.rng.chance(50));
                self.mark(What::GitDir, &m, None);
            }
            "git-file" => {
                let n = self.fresh("sep");
                let dir = self.r0.join(n);
                let store = if inside {
                    self.r0dir("store").join("r.git")
                } else {
                    self.outdir("store").join("r.git")
                };
                separate_repo(&dir, &store);
                self.mark(What::GitDir, &dir.join(".git"), Some(&dir));
                if inside {
                    self.mark(What::GitDir, &store, None);
                    self.mark(What::Pinned, &store, None);
                }
                self.git_live = false;
            }
            "dir-link" | "file-link" => {
                let repo = self.r0dir("sym");
                self.plain_repo(&repo);
                let target = if kind == "dir-link" {
                    let t = if inside {
                        self.r0dir("shared")
                    } else {
                        self.outdir("shared")
                    };
                    content(&t);
                    t
                } else {
                    let d = if inside {
                        self.r0dir("tools")
                    } else {
                        self.outdir("tools")
                    };
                    content(&d);
                    let f = d.join("hook.sh");
                    write(&f, "#!/bin/sh\nexit 0\n");
                    f
                };
                if inside {
                    self.mark(What::Pinned, &target, None);
                }
                let at = if kind == "file-link" {
                    repo.join(".git").join("hooks").join("pre-commit")
                } else if self.rng.chance(50) {
                    repo.join(".git")
                        .join("objects")
                        .join("info")
                        .join("shared")
                } else {
                    repo.join(".git").join("shared")
                };
                fs::create_dir_all(at.parent().unwrap()).unwrap();
                symlink(&target, &at).unwrap();
            }
            "core-worktree" => {
                let repo = self.r0dir("cw");
                self.plain_repo(&repo);
                let t = if inside {
                    self.r0dir("wtt")
                } else {
                    self.outdir("wtt")
                };
                content(&t);
                git(&repo, &["config", "core.worktree", s(&t)]);
                if inside {
                    self.mark(What::Pinned, &t, None);
                }
                // AC-19: a repository whose config sets core.worktree asks and keeps.
                self.mark(What::Kept, &repo, None);
            }
            "include" | "include-if" => {
                let repo = self.r0dir("inc");
                self.plain_repo(&repo);
                let d = if inside {
                    self.r0dir("cfg")
                } else {
                    self.outdir("cfg")
                };
                let t = d.join("extra.inc");
                write(&t, "[alias]\n\tshowme = status\n");
                let text = if kind == "include" {
                    format!("[include]\n\tpath = {}\n", t.display())
                } else {
                    format!(
                        "[includeIf \"gitdir:{}/\"]\n\tpath = {}\n",
                        repo.join(".git").display(),
                        t.display()
                    )
                };
                append(&repo.join(".git").join("config"), &text);
                if inside {
                    self.mark(What::Pinned, &t, None);
                }
            }
            other => unreachable!("{other}"),
        }
    }

    // ------------------------------------------------------------------------------------------
    // Symlinks above a root: the root's spelling passes through a link.

    fn above_root(&mut self, name: &str) {
        let up = self.basedir("up");
        match name {
            "link" => {
                let real = self.wdir("real");
                content(&real.join("sub").join("d"));
                self.plain_repo(&real.join("sub").join("r"));
                symlink(&real, up.join("link")).unwrap();
                self.roots.push(up.join("link").join("sub"));
                self.control(Ctl::Loose, &real.join("sub").join("d"), &[]);
            }
            "secret-link" => {
                // `.ssh` in the root's spelling names `real` by identity (AC-1).
                let real = self.wdir("real");
                content(&real.join("sub").join("d"));
                symlink(&real, up.join(".ssh")).unwrap();
                self.roots.push(up.join(".ssh").join("sub"));
                self.mark(What::SecretFlag, &real.join("sub").join("d"), None);
            }
            "secret-real" => {
                let hidden = self.wdir("hid").join(".aws").join("inner");
                content(&hidden.join("d"));
                symlink(&hidden, up.join("link")).unwrap();
                self.roots.push(up.join("link"));
                self.mark(What::SecretFlag, &hidden.join("d"), None);
            }
            "hops" => {
                let real = self.wdir("real");
                content(&real.join("sub").join("d"));
                let hop = self.basedir("hop");
                let rel = |from: &Path, to: &Path| {
                    // Both live directly below the layout's base, or below its `w`.
                    let up_count = from.strip_prefix(&self.base).unwrap().components().count() - 1;
                    let mut r = PathBuf::new();
                    for _ in 0..up_count {
                        r.push("..");
                    }
                    r.join(to.strip_prefix(&self.base).unwrap())
                };
                let h2 = hop.join("h2");
                symlink(rel(&h2, &real), &h2).unwrap();
                let h1 = up.join("h1");
                symlink(rel(&h1, &h2), &h1).unwrap();
                self.roots.push(h1.join("sub"));
                self.control(Ctl::Loose, &real.join("sub").join("d"), &[]);
            }
            "link-repo" => {
                let real = self.wdir("real");
                let proj = real.join("proj");
                write(&proj.join("package.json"), "{}\n");
                let pkg = proj.join("node_modules").join("pkg");
                write(&pkg.join("package.json"), "{\"name\": \"pkg\"}\n");
                write_bytes(&pkg.join("index.js"), 9 * KIB);
                content(&proj.join("d"));
                let r = real.join("r");
                self.pushed_repo(&r);
                symlink(&real, up.join("link")).unwrap();
                self.roots.push(up.join("link"));
                self.control(Ctl::Noise, &proj.join("node_modules"), &[]);
                self.control(Ctl::Loose, &proj.join("d"), &[]);
                self.control(Ctl::Git, &r, &[]);
            }
            other => unreachable!("{other}"),
        }
    }

    // ------------------------------------------------------------------------------------------
    // Names that are not valid UTF-8 (Linux: other systems refuse them) and secret and git names
    // in another letter case.

    fn odd_names(&mut self, name: &str) {
        let linux = cfg!(target_os = "linux");
        let name = match name {
            "bad-utf8-file" if !linux => "case-env",
            "bad-utf8-dir" if !linux => "case-ssh",
            "bad-utf8-secret" if !linux => "case-key",
            "case-git" if !linux => "case-secrets",
            n => n,
        };
        self.tag(format!("name={name}"));
        let d = self.r0dir("odd");
        match name {
            "bad-utf8-file" => {
                content(&d);
                let f = d.join(bad_name(b"caf\xe9.bin"));
                write_bytes(&f, 100 * KIB);
                self.mark(What::Inexact, &f, None);
            }
            "bad-utf8-dir" => {
                let dd = d.join(bad_name(b"\xff-dir"));
                content(&dd);
                self.mark(What::Inexact, &dd, None);
            }
            "bad-utf8-secret" => {
                content(&d);
                // Secret-named files are hash candidates (a twin of every x.bin), so "never
                // hashed" is not vacuous.
                let f = d.join(bad_name(b"id_rsa\xff"));
                write_bytes(&f, 100 * KIB);
                self.mark(What::Inexact, &f, None);
                self.mark(What::SecretFlag, &f, None);
            }
            "case-env" => {
                content(&d);
                for n in [".ENV", ".Env.Local"] {
                    let f = d.join(n);
                    write_bytes(&f, 100 * KIB);
                    self.mark(What::SecretFlag, &f, None);
                }
            }
            "case-ssh" => {
                let dd = d.join(".Ssh");
                content(&dd);
                write(&dd.join("id_x"), "not a key\n");
                self.mark(What::SecretFlag, &dd, None);
            }
            "case-secrets" => {
                let dd = d.join("Secrets");
                content(&dd);
                self.mark(What::SecretFlag, &dd, None);
            }
            "case-key" => {
                content(&d);
                for n in ["ID_RSA", "Server.PEM"] {
                    let f = d.join(n);
                    write_bytes(&f, 100 * KIB);
                    self.mark(What::SecretFlag, &f, None);
                }
            }
            "case-git" => {
                init_bare_at(&d.join(".Git"));
                content(&d);
                self.mark(What::GitDir, &d.join(".Git"), None);
                self.mark(What::Foreign, &d, None);
            }
            other => unreachable!("{other}"),
        }
    }

    // ------------------------------------------------------------------------------------------
    // Secret places.

    fn secret_place(&mut self, name: &str) {
        match name {
            "ssh-dir" => {
                let ssh = self.r0dir("home").join(".ssh");
                content(&ssh);
                write(&ssh.join("id_ed25519"), "not a key\n");
                write(&ssh.join("config"), "Host example\n");
                self.mark(What::SecretFlag, &ssh, None);
            }
            "password-store-repo" => {
                let ps = self.r0dir("home").join(".password-store");
                self.secret_repo(&ps);
            }
            "secrets-repo" => {
                let sd = self.r0dir("s").join("secrets");
                content(&sd.join("notes"));
                self.mark(What::SecretFlag, &sd.join("notes"), None);
                self.secret_repo(&sd.join("proj"));
            }
            "secret-named-links" => {
                // NEG-2: `.aws` and `.env` links make their targets secret.
                let home = self.r0dir("home");
                let vault_name = self.fresh("vault");
                let vault = self.r0.join(&vault_name);
                content(&vault.join("aws"));
                write_bytes(&vault.join("settings.txt"), 100 * KIB);
                fs::create_dir_all(&home).unwrap();
                symlink(format!("../{vault_name}/aws"), home.join(".aws")).unwrap();
                symlink(format!("../{vault_name}/settings.txt"), home.join(".env")).unwrap();
                self.mark(What::SecretFlag, &vault.join("aws"), None);
                self.mark(What::SecretFlag, &vault.join("settings.txt"), None);
                self.mark(What::SecretFlag, &home.join(".env"), None);
            }
            "link-in-secret-dir" => {
                // NEG-12: an ordinary name in secret context.
                let ssh = self.r0dir("home").join(".ssh");
                write(&ssh.join("known_hosts"), "example\n");
                let vault_name = self.fresh("vault");
                let vault = self.r0.join(&vault_name);
                content(&vault.join("work"));
                symlink(format!("../../{vault_name}/work"), ssh.join("work")).unwrap();
                self.mark(What::SecretFlag, &ssh, None);
                self.mark(What::SecretFlag, &vault.join("work"), None);
            }
            "aws-link-to-parent" => {
                // NEG-12: `.aws -> ..`, a target above the link.
                let u = self.r0dir("up");
                content(&u.join("data"));
                fs::create_dir_all(u.join("sub")).unwrap();
                symlink("..", u.join("sub").join(".aws")).unwrap();
                self.mark(What::SecretFlag, &u, None);
            }
            "repo-through-aws-root" => {
                // NEG-7: a repository secret only through the spelling of a second root.
                let plain = self.wdir("plain");
                content(&plain.join("d"));
                self.secret_repo(&plain.join("r2"));
                let a = self.basedir("a").join(".aws");
                symlink(&plain, &a).unwrap();
                self.roots.push(plain.clone());
                self.roots.push(a);
                self.mark(What::SecretFlag, &plain.join("d"), None);
            }
            "hard-linked-env" => {
                // NEG-18: a secret-named file hard-linked under an ordinary name.
                let d = self.r0dir("hl");
                content(&d);
                write_bytes(&d.join(".env"), 100 * KIB);
                fs::hard_link(d.join(".env"), d.join("data.bin")).unwrap();
                self.mark(What::SecretFlag, &d.join(".env"), None);
            }
            "secret-entry-in-artifact" => {
                // NEG-30: the secret entry stays in place, the rest of the artifact is noise.
                let proj = self.r0dir("proj");
                write(&proj.join("package.json"), "{}\n");
                write(&proj.join("requirements.txt"), "certifi\n");
                let nm = proj.join("node_modules");
                let pkg = nm.join("pkg");
                write(&pkg.join("package.json"), "{\"name\": \"pkg\"}\n");
                write_bytes(&pkg.join("index.js"), 9 * KIB);
                write(
                    &pkg.join(".npmrc"),
                    "registry=https://registry.example.invalid/\n",
                );
                let venv = proj.join("venv");
                write(&venv.join("pyvenv.cfg"), "home = /usr/bin\n");
                let site = venv.join("lib").join("python3.11").join("site-packages");
                write(&site.join("pip").join("__init__.py"), "\n");
                let certifi = site.join("pip").join("_vendor").join("certifi");
                write(&certifi.join("cacert.pem"), "not a bundle\n");
                write_bytes(&certifi.join("core.py"), 9 * KIB);
                self.mark(What::SecretFlag, &pkg.join(".npmrc"), None);
                self.mark(What::SecretFlag, &certifi.join("cacert.pem"), None);
                self.control(Ctl::Noise, &nm, &[pkg.join(".npmrc")]);
                self.control(Ctl::Noise, &venv, &[certifi.join("cacert.pem")]);
            }
            other => unreachable!("{other}"),
        }
    }
}

#[cfg(target_os = "linux")]
fn bad_name(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(target_os = "linux"))]
fn bad_name(_: &[u8]) -> PathBuf {
    unreachable!("names that are not valid UTF-8 are built on Linux only")
}

/// The primary feature of layout `i`.
fn primary(i: usize) -> (Dim, usize) {
    let dim = DIMS[i % DIMS.len()];
    (dim, (i / DIMS.len()) % dim.variants().len())
}

fn label(base: &Path, p: &Path) -> String {
    p.strip_prefix(base)
        .map(|r| r.display().to_string())
        .unwrap_or_else(|_| p.display().to_string())
}

fn build(index: usize) -> Layout {
    let seed = layout_seed(index);
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let mut b = B::new(base.clone(), seed);
    let (dim, v) = primary(index);
    b.tag(format!("primary={}/{}", dim.name(), dim.variants()[v]));
    b.feature(dim, v);
    let mut extra = 0;
    while extra < 2 && b.rng.chance(if extra == 0 { 50 } else { 25 }) {
        // Root order is primary only: its three roots are placed in a chosen relative order.
        let others: Vec<Dim> = DIMS
            .iter()
            .copied()
            .filter(|d| *d != Dim::RootOrder && *d != dim)
            .collect();
        let d = others[b.rng.below(others.len())];
        let v = b.rng.below(d.variants().len());
        b.tag(format!("secondary={}/{}", d.name(), d.variants()[v]));
        b.feature(d, v);
        extra += 1;
    }
    let po = if b.small {
        b.tag("pieces=small".into());
        PieceOptions {
            max_piece_bytes: 64 * KIB as u64,
            ..PieceOptions::default()
        }
    } else {
        PieceOptions::default()
    };
    // Roots in a random order; the root-order triple takes its chosen relative order.
    let mut roots = b.roots.clone();
    if let Some((t, _)) = &b.triple {
        roots.extend(t.iter().cloned());
    }
    b.rng.shuffle(&mut roots);
    if let Some((t, perm)) = b.triple.clone() {
        let slots: Vec<usize> = (0..roots.len())
            .filter(|i| t.contains(&roots[*i]))
            .collect();
        assert_eq!(
            slots.len(),
            3,
            "generator: the root-order triple is distinct"
        );
        for (k, slot) in slots.into_iter().enumerate() {
            roots[slot] = t[perm[k]].clone();
        }
        let names: Vec<String> = roots
            .iter()
            .filter(|r| t.contains(r))
            .map(|r| label(&base, r))
            .collect();
        b.tag(format!("triple-order={}", names.join(",")));
    }
    let real_roots = roots
        .iter()
        .map(|r| {
            fs::canonicalize(r).unwrap_or_else(|e| panic!("generator: root {}: {e}", r.display()))
        })
        .collect();
    // The observers live in the layout but outside every root.
    let tools = base.join("tools");
    fs::create_dir_all(&tools).unwrap();
    let log = tools.join("git.log");
    let real_git = String::from_utf8(
        std::process::Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let wrapper = script(
        &tools,
        "git-recorder",
        &format!(
            "#!/bin/sh\nprintf '%s %s\\n' \"$(pwd -P)\" \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real_git
        ),
    );
    // Identities, after everything is built.
    let mut places = Vec::new();
    for (what, path, owner) in &b.places {
        let ids = ids_under(path);
        assert!(
            !ids.files.is_empty(),
            "generator: place {} has no entries",
            path.display()
        );
        let owner = owner.as_ref().map(|o| {
            fid(o).unwrap_or_else(|| panic!("generator: owner {} is missing", o.display()))
        });
        places.push(Place {
            what: *what,
            label: label(&base, path),
            owner,
            ids,
        });
    }
    let mut controls = Vec::new();
    for (kind, path, exclude) in &b.controls {
        let mut ids = ids_under(path);
        for x in exclude {
            for id in ids_under(x).files {
                ids.files.remove(&id);
            }
        }
        controls.push(Control {
            kind: *kind,
            label: label(&base, path),
            ids,
        });
    }
    // The generator checks itself: no control overlaps a place that must not be removed.
    for c in &controls {
        for p in places
            .iter()
            .filter(|p| !(p.what == What::GitDir && p.owner.is_some()))
        {
            assert!(
                c.ids.files.is_disjoint(&p.ids.files),
                "generator: control {} overlaps {:?} place {}",
                c.label,
                p.what,
                p.label
            );
        }
    }
    let no_git = b
        .no_git
        .iter()
        .map(|r| (label(&base, r), ids_under(r)))
        .collect();
    Layout {
        index,
        seed,
        dims: b.dims.clone(),
        tags: b.tags.clone(),
        _tmp: tmp,
        base,
        roots,
        real_roots,
        po,
        wrapper,
        log,
        places,
        no_git,
        controls,
        git_live: b.git_live,
    }
}

// ----------------------------------------------------------------------------------------------
// Planning and observation.

/// The plan as the command line makes it: from the scan file written and read back, never from
/// the scan held in memory, so a protection kept only in memory cannot pass. It must also equal
/// the plan made in memory (AC-11).
fn idle_plan(res: &ScanResult, bad: &mut Vec<(&'static str, String)>) -> Plan {
    let opts = PlanOptions::new(7, far_future());
    let text = serde_json::to_string_pretty(res).expect("scan file");
    let back = scan_from_json(&text).expect("the scan file reads back");
    let from_file = plan(&back, &opts);
    let in_memory = plan(res, &opts);
    for (what, f, m) in [
        (
            "lanes",
            serde_json::to_value(&from_file.lanes).unwrap(),
            serde_json::to_value(&in_memory.lanes).unwrap(),
        ),
        (
            "items",
            serde_json::to_value(&from_file.items).unwrap(),
            serde_json::to_value(&in_memory.items).unwrap(),
        ),
    ] {
        if f != m {
            bad.push((
                "round-trip",
                format!("the plan's {what} from the scan file differ from those in memory"),
            ));
        }
    }
    from_file
}

/// One member of a plan piece: its identity, its emitted path and its piece.
struct Mem<'a> {
    id: Id,
    path: String,
    lp: &'a LanePiece,
}

fn members<'a>(res: &ScanResult, p: &'a Plan) -> Vec<Mem<'a>> {
    let (Some(survey), Some(lanes)) = (res.pieces.as_ref(), p.lanes.as_ref()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for lp in &lanes.pieces {
        let sp = &survey.pieces[lp.scan_piece];
        for m in &sp.members {
            let keep = match &lp.member {
                Some(one) => m.path() == one,
                None => !lp.moved_out.contains(&m.0),
            };
            if keep {
                out.push(Mem {
                    id: (m.5, m.6),
                    path: format!("{}/{}", sp.path, m.path()),
                    lp,
                });
            }
        }
    }
    out
}

fn real_or_raw(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The problem with an emitted path, if any (AC-16): every component is `lstat`ed.
fn path_problem(
    p: &str,
    real_roots: &[PathBuf],
    cache: &mut HashMap<PathBuf, Option<bool>>,
) -> Option<(&'static str, String)> {
    let path = Path::new(p);
    let mut prefix = PathBuf::new();
    for c in path.components() {
        match c {
            Component::RootDir => prefix.push("/"),
            Component::Normal(n) => {
                prefix.push(n);
                let link =
                    *cache.entry(prefix.clone()).or_insert_with(|| {
                        match fs::symlink_metadata(&prefix) {
                            Ok(md) => Some(md.file_type().is_symlink()),
                            Err(_) => None,
                        }
                    });
                match link {
                    Some(true) => {
                        return Some((
                            "c:symlinked-component",
                            format!("{p}: {} is a symlink", prefix.display()),
                        ))
                    }
                    Some(false) => {}
                    // A lossy spelling of a name that is not UTF-8 cannot be looked up.
                    None if p.contains('\u{fffd}') => return None,
                    None => {
                        return Some((
                            "c:missing",
                            format!("{p}: {} does not exist", prefix.display()),
                        ))
                    }
                }
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Some(("c:dot-component", format!("{p}: not a real path")));
            }
        }
    }
    if !path.is_absolute() {
        return Some(("c:relative", p.to_string()));
    }
    if !real_roots.iter().any(|r| path.starts_with(r)) {
        return Some((
            "c:outside-roots",
            format!("{p} is outside the roots' real paths"),
        ));
    }
    None
}

fn check(l: &Layout, bad: &mut Vec<(&'static str, String)>) {
    let mut opts = ScanOptions::new(l.roots.clone());
    opts.facts = network_everywhere().with_git(Git::new(&l.wrapper, Duration::from_secs(60)));
    opts.git_jobs = 2;
    opts.lanes = Some(l.po.clone());
    let res = match scan(&opts) {
        Ok(r) => r,
        Err(e) => {
            bad.push(("error", format!("scan failed: {e}")));
            return;
        }
    };
    let p = idle_plan(&res, bad);
    let Some(survey) = res.pieces.as_ref() else {
        bad.push(("error", "the scan has no pieces".into()));
        return;
    };
    if p.lanes.is_none() {
        bad.push(("error", "the plan has no lanes".into()));
        return;
    }
    let all = members(&res, &p);

    // (a), (g) and the controls, by identity.
    let mut by_id: HashMap<Id, Vec<usize>> = HashMap::new();
    for (k, pl) in l.places.iter().enumerate() {
        for id in &pl.ids.files {
            by_id.entry(*id).or_default().push(k);
        }
    }
    let mut matched = vec![0usize; l.places.len()];
    for m in &all {
        let Some(ks) = by_id.get(&m.id) else {
            continue;
        };
        for &k in ks {
            let pl = &l.places[k];
            matched[k] += 1;
            let removes = m.lp.lane.removes();
            // Only a git directory may go, and only as its own repository's git-node piece.
            let own_git_piece = pl.what == What::GitDir
                && m.lp.kind == PieceKind::GitNode
                && pl.owner.is_some()
                && fid(Path::new(&m.lp.path)) == pl.owner;
            let ok = !removes || own_git_piece;
            if !ok {
                bad.push((
                    pl.what.invariant(),
                    format!(
                        "{} ({:?} {}) is {:?} in {:?} piece {}: {}",
                        m.path, pl.what, pl.label, m.lp.lane, m.lp.kind, m.lp.path, m.lp.reason
                    ),
                ));
            } else if pl.what == What::SecretFlag && m.lp.secret.is_none() {
                bad.push((
                    "a:secret-not-flagged",
                    format!(
                        "{} ({}) is {:?} but not a secret piece: {}",
                        m.path, pl.label, m.lp.lane, m.lp.reason
                    ),
                ));
            }
        }
    }
    for (k, pl) in l.places.iter().enumerate() {
        if matched[k] == 0 {
            bad.push((
                "oracle:vacuous",
                format!("{:?} place {} matched no member", pl.what, pl.label),
            ));
        }
    }

    // (b) and (g): what was opened and hashed.
    let mut secret_files: HashSet<Id> = HashSet::new();
    let mut inexact_files: HashSet<Id> = HashSet::new();
    for pl in &l.places {
        match pl.what {
            What::Secret | What::SecretFlag => secret_files.extend(pl.ids.files.iter().copied()),
            What::Inexact => inexact_files.extend(pl.ids.files.iter().copied()),
            What::GitDir | What::Foreign | What::Pinned | What::Kept => {}
        }
    }
    for o in testing::opens_under(&l.base) {
        if let Some(id) = lid(&o) {
            if secret_files.contains(&id) {
                bad.push(("b:secret-opened", format!("{} was opened", o.display())));
            }
            if inexact_files.contains(&id) {
                bad.push(("g:inexact-opened", format!("{} was opened", o.display())));
            }
        }
    }
    let mut hashed: Vec<Id> = Vec::new();
    for c in survey.blobs.iter().flat_map(|b| b.copies.iter()) {
        match lid(Path::new(&c.path)) {
            Some(id) => {
                hashed.push(id);
                if secret_files.contains(&id) {
                    bad.push(("b:secret-hashed", format!("{} was hashed", c.path)));
                }
                if inexact_files.contains(&id) {
                    bad.push(("g:inexact-hashed", format!("{} was hashed", c.path)));
                }
            }
            None if c.path.contains('\u{fffd}') => {
                bad.push((
                    "g:inexact-hashed",
                    format!("{} (not UTF-8) was hashed", c.path),
                ));
            }
            None => {}
        }
    }

    // (b): git processes, by the identity of their working directory and path arguments.
    let ran = fs::read_to_string(&l.log).unwrap_or_default();
    let git_ctl: Vec<&Control> = l.controls.iter().filter(|c| c.kind == Ctl::Git).collect();
    let mut ctl_ran = vec![false; git_ctl.len()];
    for line in ran.lines() {
        let mut words = line.split_whitespace();
        let Some(cwd) = words.next() else {
            continue;
        };
        let cwd_id = fid(Path::new(cwd));
        for (k, c) in git_ctl.iter().enumerate() {
            if cwd_id.is_some_and(|id| c.ids.dirs.contains(&id)) {
                ctl_ran[k] = true;
            }
        }
        let args: Vec<&str> = words
            .map(|w| w.rsplit('=').next().unwrap_or(w))
            .filter(|w| w.starts_with('/'))
            .collect();
        for (repo, ids) in &l.no_git {
            let hit_cwd = cwd_id.is_some_and(|id| ids.dirs.contains(&id));
            let hit_arg = args.iter().any(|a| {
                let p = Path::new(a);
                fid(p).is_some_and(|id| ids.dirs.contains(&id) || ids.files.contains(&id))
                    || lid(p).is_some_and(|id| ids.files.contains(&id))
            });
            if hit_cwd || hit_arg {
                bad.push(("b:git-ran", format!("git ran for {repo}: {line}")));
            }
        }
    }

    // (c): every emitted path is real.
    let lanes = p.lanes.as_ref().unwrap();
    let mut emitted: BTreeMap<String, &'static str> = BTreeMap::new();
    for r in &res.roots {
        emitted.entry(r.clone()).or_insert("root");
    }
    for n in &res.nodes {
        emitted.entry(n.path.clone()).or_insert("node");
    }
    for it in &p.items {
        emitted.entry(it.path.clone()).or_insert("item");
    }
    for sp in &survey.pieces {
        emitted.entry(sp.path.clone()).or_insert("scan piece");
    }
    for lp in &lanes.pieces {
        emitted.entry(lp.path.clone()).or_insert("plan piece");
    }
    for c in survey.blobs.iter().flat_map(|b| b.copies.iter()) {
        emitted.entry(c.path.clone()).or_insert("blob copy");
    }
    for b in &lanes.blobs {
        for c in &b.copies {
            emitted.entry(c.path.clone()).or_insert("plan blob copy");
        }
        if let Some(u) = &b.upload_path {
            emitted.entry(u.clone()).or_insert("upload path");
        }
    }
    let mut cache = HashMap::new();
    for (path, what) in &emitted {
        if let Some((inv, text)) = path_problem(path, &l.real_roots, &mut cache) {
            bad.push((inv, format!("{what} {text}")));
        }
    }

    // (d): the survey and the main walk count the same entries and bytes.
    if survey.totals.entries != res.totals.files {
        bad.push((
            "d:files",
            format!(
                "survey entries {} but the main walk counted {} files",
                survey.totals.entries, res.totals.files
            ),
        ));
    }
    if survey.totals.bytes + survey.totals.dir_overhead_bytes != res.totals.allocated_bytes {
        bad.push((
            "d:bytes",
            format!(
                "survey bytes {} plus overhead {} but the main walk counted {}",
                survey.totals.bytes, survey.totals.dir_overhead_bytes, res.totals.allocated_bytes
            ),
        ));
    }

    // (e): AC-21, a removing item holds no kept piece but a secret one.
    let reals: Vec<PathBuf> = all
        .iter()
        .map(|m| real_or_raw(Path::new(&m.path)))
        .collect();
    for it in p.items.iter().filter(|i| i.action.removes()) {
        let at = real_or_raw(Path::new(&it.path));
        for (m, r) in all.iter().zip(&reals) {
            if r.starts_with(&at) && m.lp.lane == Lane::Keep && m.lp.secret.is_none() {
                bad.push((
                    "e:item-removes-a-kept-piece",
                    format!(
                        "item {} is {:?} but {} is kept: {}",
                        it.path, it.action, m.path, m.lp.reason
                    ),
                ));
            }
        }
    }

    // (f): the controls.
    for c in &l.controls {
        let under: Vec<&Mem> = all.iter().filter(|m| c.ids.files.contains(&m.id)).collect();
        if under.is_empty() {
            bad.push((
                "f:control-missing",
                format!("{:?} control {} has no member", c.kind, c.label),
            ));
        }
        match c.kind {
            Ctl::Loose => {
                for m in &under {
                    if m.lp.lane != Lane::Backup && m.lp.lane != Lane::Noise {
                        bad.push((
                            "f:loose-control",
                            format!("{} is {:?}: {}", m.path, m.lp.lane, m.lp.reason),
                        ));
                    }
                }
                if hashed.iter().filter(|id| c.ids.files.contains(id)).count() < 2 {
                    bad.push((
                        "f:loose-control-not-hashed",
                        format!("control {} was not hashed", c.label),
                    ));
                }
            }
            Ctl::Noise => {
                for m in &under {
                    if m.lp.lane != Lane::Noise {
                        bad.push((
                            "f:noise-control",
                            format!("{} is {:?}: {}", m.path, m.lp.lane, m.lp.reason),
                        ));
                    }
                }
            }
            Ctl::Git => {
                if l.git_live {
                    for m in &under {
                        if m.lp.lane != Lane::Git {
                            bad.push((
                                "f:git-control",
                                format!("{} is {:?}: {}", m.path, m.lp.lane, m.lp.reason),
                            ));
                        }
                    }
                }
            }
        }
    }
    for (k, c) in git_ctl.iter().enumerate() {
        if !ctl_ran[k] {
            bad.push((
                "f:git-control-not-run",
                format!("git did not run for {}", c.label),
            ));
        }
    }
}

// ----------------------------------------------------------------------------------------------
// The test.

fn panic_text(e: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = e.downcast_ref::<&str>() {
        (*s).to_string()
    } else {
        "a panic without a message".to_string()
    }
}

/// Run `f` over `items` on `threads` threads; results in item order.
fn par_map<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|sc| {
        for _ in 0..threads.max(1) {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= items.len() {
                    break;
                }
                let r = f(&items[i]);
                out.lock().unwrap()[i] = Some(r);
            });
        }
    });
    out.into_inner()
        .unwrap()
        .into_iter()
        .map(|r| r.expect("every item ran"))
        .collect()
}

struct Found {
    index: usize,
    seed: u64,
    dims: Vec<Dim>,
    tags: Vec<String>,
    invariant: &'static str,
    text: String,
}

#[test]
#[ignore = "red at f64a075: the guard drives the rescue's slices; enabled at the rescue head (goodqa#16529)"]
fn guard_composed_layouts_hold_every_invariant() {
    let started = Instant::now();
    testing::record_opens();
    let total = SEEDS.len() * PER_SEED;
    let only: Option<BTreeSet<usize>> = std::env::var("GUARD_ONLY").ok().map(|v| {
        v.split(',')
            .map(|x| x.trim().parse().expect("GUARD_ONLY: layout indexes"))
            .collect()
    });
    let indexes: Vec<usize> = (0..total)
        .filter(|i| only.as_ref().is_none_or(|o| o.contains(i)))
        .collect();
    println!(
        "guard: seeds {:#018x} {:#018x}, {} per seed, {} layouts to run",
        SEEDS[0],
        SEEDS[1],
        PER_SEED,
        indexes.len()
    );

    // Build every layout first, then wait once: status-change times at or after the survey's
    // start count as "changed after the scan".
    let built: Vec<Result<Layout, Found>> = par_map(&indexes, BUILD_THREADS, |&i| {
        catch_unwind(AssertUnwindSafe(|| build(i))).map_err(|e| {
            let (dim, v) = primary(i);
            Found {
                index: i,
                seed: layout_seed(i),
                dims: vec![dim],
                tags: vec![format!("primary={}/{}", dim.name(), dim.variants()[v])],
                invariant: "error",
                text: format!("building the layout failed: {}", panic_text(e)),
            }
        })
    });
    let built_in = started.elapsed();
    std::thread::sleep(Duration::from_millis(1100));

    let mut found: Vec<Found> = Vec::new();
    let mut layouts: Vec<Layout> = Vec::new();
    for b in built {
        match b {
            Ok(l) => layouts.push(l),
            Err(f) => found.push(f),
        }
    }
    let scan_started = Instant::now();
    let results: Vec<Vec<(&'static str, String)>> = par_map(&layouts, SCAN_THREADS, |l| {
        let mut bad = Vec::new();
        if let Err(e) = catch_unwind(AssertUnwindSafe(|| check(l, &mut bad))) {
            bad.push((
                "error",
                format!("checking the layout panicked: {}", panic_text(e)),
            ));
        }
        bad
    });
    let scanned_in = scan_started.elapsed();

    // Coverage of the dimensions and variants, counted from the layouts as built.
    let mut per_dim: BTreeMap<Dim, usize> = BTreeMap::new();
    let mut per_variant: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_tag: BTreeMap<String, usize> = BTreeMap::new();
    let mut live = 0usize;
    for l in &layouts {
        for d in &l.dims {
            *per_dim.entry(*d).or_default() += 1;
        }
        for t in &l.tags {
            if let Some(v) = t
                .strip_prefix("primary=")
                .or_else(|| t.strip_prefix("secondary="))
            {
                *per_variant.entry(v.to_string()).or_default() += 1;
            } else if !t.starts_with("triple-order=") {
                *per_tag.entry(t.clone()).or_default() += 1;
            }
        }
        if l.git_live {
            live += 1;
        }
    }
    let orders: BTreeSet<String> = layouts
        .iter()
        .flat_map(|l| l.tags.iter())
        .filter_map(|t| t.strip_prefix("triple-order="))
        .map(|t| {
            // The relative order as p/kid/alias, whatever the layout's names.
            t.split(',')
                .map(|n| {
                    if n.ends_with("/kid") {
                        "kid"
                    } else if n.contains("/ap") || n.starts_with("w/ap") {
                        "alias"
                    } else {
                        "parent"
                    }
                })
                .collect::<Vec<_>>()
                .join(">")
        })
        .collect();
    println!(
        "guard: built {} layouts in {:.1}s, scanned and checked in {:.1}s ({} scan threads)",
        layouts.len(),
        built_in.as_secs_f64(),
        scanned_in.as_secs_f64(),
        SCAN_THREADS
    );
    println!("guard: layouts per dimension:");
    for d in DIMS {
        println!(
            "  {:32} {}",
            d.name(),
            per_dim.get(&d).copied().unwrap_or(0)
        );
    }
    println!("guard: layouts per variant (primary or secondary):");
    for (v, n) in &per_variant {
        println!("  {v:60} {n}");
    }
    println!("guard: other choices: {per_tag:?}");
    println!(
        "guard: root-order relative orders seen: {} {:?}",
        orders.len(),
        orders
    );
    println!(
        "guard: git control required in the git lane in {live} of {} layouts",
        layouts.len()
    );

    for (l, bad) in layouts.iter().zip(results) {
        for (invariant, text) in bad {
            found.push(Found {
                index: l.index,
                seed: l.seed,
                dims: l.dims.iter().copied().collect(),
                tags: l.tags.clone(),
                invariant,
                text,
            });
        }
    }

    // Coverage the test itself must reach (never relaxed by a filter: a filtered run fails).
    let mut meta = Vec::new();
    if only.is_some() {
        meta.push("GUARD_ONLY is set: a partial run never passes".to_string());
    } else {
        if layouts.len() < MIN_LAYOUTS {
            meta.push(format!(
                "only {} layouts ran, at least {MIN_LAYOUTS} must",
                layouts.len()
            ));
        }
        for d in DIMS {
            let n = per_dim.get(&d).copied().unwrap_or(0);
            if n < MIN_PER_DIMENSION {
                meta.push(format!(
                    "dimension {} has {n} layouts, at least {MIN_PER_DIMENSION} must",
                    d.name()
                ));
            }
            for v in d.variants() {
                let key = format!("{}/{v}", d.name());
                let n = per_variant.get(&key).copied().unwrap_or(0);
                if n < MIN_PER_VARIANT {
                    meta.push(format!(
                        "variant {key} has {n} layouts, at least {MIN_PER_VARIANT} must"
                    ));
                }
            }
        }
        if orders.len() < 6 {
            meta.push(format!(
                "only {} of the 6 relative root orders were built",
                orders.len()
            ));
        }
        if live * 4 < layouts.len() {
            meta.push(format!(
                "the git control was required in only {live} of {} layouts",
                layouts.len()
            ));
        }
    }

    // The summary: counts per invariant and per dimension, then examples of each invariant.
    let mut per_inv: BTreeMap<&str, (usize, BTreeSet<usize>)> = BTreeMap::new();
    let mut per_inv_dim: BTreeMap<(&str, Dim), usize> = BTreeMap::new();
    for f in &found {
        let e = per_inv.entry(f.invariant).or_default();
        e.0 += 1;
        e.1.insert(f.index);
        for d in &f.dims {
            *per_inv_dim.entry((f.invariant, *d)).or_default() += 1;
        }
    }
    let bad_layouts: BTreeSet<usize> = found.iter().map(|f| f.index).collect();
    println!(
        "guard: {} violations in {} of {} layouts",
        found.len(),
        bad_layouts.len(),
        indexes.len()
    );
    println!("guard: violations per invariant (violations, layouts):");
    for (inv, (n, ls)) in &per_inv {
        println!("  {inv:32} {n:6} {:4}", ls.len());
    }
    println!("guard: violations per invariant and dimension:");
    for ((inv, d), n) in &per_inv_dim {
        println!("  {inv:32} {:32} {n}", d.name());
    }
    // Attribution: layouts made of one feature only, per variant, so each violation class can be
    // matched to the feature that shows it.
    let single: BTreeMap<usize, String> = layouts
        .iter()
        .filter(|l| !l.tags.iter().any(|t| t.starts_with("secondary=")))
        .filter_map(|l| {
            l.tags
                .iter()
                .find_map(|t| t.strip_prefix("primary="))
                .map(|v| (l.index, v.to_string()))
        })
        .collect();
    let mut single_total: BTreeMap<&str, usize> = BTreeMap::new();
    for v in single.values() {
        *single_total.entry(v.as_str()).or_default() += 1;
    }
    let mut single_bad: BTreeMap<&str, BTreeMap<&str, BTreeSet<usize>>> = BTreeMap::new();
    for f in &found {
        if let Some(v) = single.get(&f.index) {
            single_bad
                .entry(v.as_str())
                .or_default()
                .entry(f.invariant)
                .or_default()
                .insert(f.index);
        }
    }
    println!("guard: single-feature layouts per variant (layouts; invariant=layouts violating):");
    for (v, n) in &single_total {
        let detail: Vec<String> = single_bad
            .get(v)
            .map(|m| {
                m.iter()
                    .map(|(inv, ls)| format!("{inv}={}", ls.len()))
                    .collect()
            })
            .unwrap_or_default();
        println!("  {v:60} {n:3}; {}", detail.join(" "));
    }
    // Round robin over the invariants, so every kind gets examples.
    let mut queues: BTreeMap<&str, Vec<&Found>> = BTreeMap::new();
    for f in &found {
        queues.entry(f.invariant).or_default().push(f);
    }
    let mut examples = Vec::new();
    let mut round = 0;
    while examples.len() < MAX_EXAMPLES {
        let before = examples.len();
        for q in queues.values() {
            if let Some(f) = q.get(round) {
                if examples.len() < MAX_EXAMPLES {
                    examples.push(*f);
                }
            }
        }
        if examples.len() == before {
            break;
        }
        round += 1;
    }
    println!("guard: examples ({} of {}):", examples.len(), found.len());
    for f in &examples {
        println!(
            "  [{}] layout {} seed {:#018x} {:?}: {}",
            f.invariant, f.index, f.seed, f.tags, f.text
        );
    }
    println!(
        "guard: reproduce one layout with GUARD_ONLY=<layout> sh crates/core/tests/verify.sh guard"
    );
    println!("guard: finished in {:.1}s", started.elapsed().as_secs_f64());
    assert!(
        found.is_empty() && meta.is_empty(),
        "the guard found {} violations in {} layouts{}{}",
        found.len(),
        bad_layouts.len(),
        if meta.is_empty() { "" } else { "; coverage: " },
        meta.join("; ")
    );
}
