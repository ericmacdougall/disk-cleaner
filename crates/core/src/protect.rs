//! Protection of agent data, cloud-sync folders and application packages (see `docs/DESIGN.md`).
//!
//! Two halves. [`resolve`] runs at scan time: it checks the process context it is given (user,
//! home, environment overrides, all passed in so tests can pass them explicitly) and turns the
//! built-in roots, the environment overrides and the config's `protected_paths` into the roots
//! recorded in the scan. [`judge`] is pure: it decides, from the scan alone (no file system or
//! git calls), which nodes are held and which sit in a carve-out position. The plan applies the
//! verdicts before its nested-veto pass.
//!
//! Threat model: careless agents and ordinary mistakes, not an adversary with write access to
//! the home directory.

use crate::error::{Error, Result};
use crate::model::*;
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

/// Built-in protected roots, relative to the home, and where a clean checkout inside may be
/// removed after a backup.
pub const BUILTIN_PROTECTED: [(&str, CarveLayout); 11] = [
    (".claude", CarveLayout::Worktrees),
    (".codex", CarveLayout::Worktrees),
    (".agent-harness", CarveLayout::AgentHarness),
    (".prunedisk", CarveLayout::None),
    (".kimi", CarveLayout::None),
    (".kimi-code", CarveLayout::None),
    (".gemini", CarveLayout::None),
    (".cursor", CarveLayout::None),
    (".config/claude", CarveLayout::None),
    (".local/share/claude", CarveLayout::None),
    ("Library/Application Support/Claude", CarveLayout::None),
];

/// Cloud-sync folders at fixed places below the home.
pub const SYNCED_FIXED: [&str; 2] = ["Library/Mobile Documents", "Library/CloudStorage"];

/// Cloud-sync folders directly in the home whose name starts with one of these.
pub const SYNCED_PREFIXES: [&str; 4] = ["Dropbox", "Google Drive", "OneDrive", "iCloud Drive"];

/// A directory whose name ends with one of these is a package: nothing inside it is a candidate.
/// (`vendor/bundle` and the like are not packages: only these suffixes count.)
pub const PACKAGE_SUFFIXES: [&str; 13] = [
    ".app",
    ".photoslibrary",
    ".fcpbundle",
    ".logicx",
    ".band",
    ".lrlibrary",
    ".lrcat-data",
    ".musiclibrary",
    ".tvlibrary",
    ".framework",
    ".vmwarevm",
    ".pvm",
    ".utm",
];

/// The process context a scan resolves its protected roots from. Everything is an input; the CLI
/// fills it from the real process, tests pass values explicitly.
#[derive(Debug, Clone, Default)]
pub struct ScanContext {
    /// The scanning user's home from the passwd entry.
    pub passwd_home: Option<PathBuf>,
    /// Effective user id of the process, when known.
    pub euid: Option<u32>,
    /// `HOME` as seen.
    pub home_env: Option<String>,
    /// `--home DIR`: the home to resolve under instead of the passwd home (disposable fixture
    /// homes). `HOME` must still be the same directory, and the passwd home stays protected too.
    pub declared_home: Option<PathBuf>,
    pub claude_config_dir: Option<String>,
    pub codex_home: Option<String>,
    pub xdg_config_home: Option<String>,
    /// The config's `protected_paths` (absolute or `~/`-relative).
    pub extra_paths: Vec<String>,
}

impl ScanContext {
    /// The environment overrides as the process sees them. The user ids and the passwd home are
    /// not available to the safe core (it forbids unsafe code), so the CLI supplies them; a
    /// library caller that does not gets the `HOME` fallback and no user checks.
    pub fn from_env(extra_paths: Vec<String>) -> ScanContext {
        let var = |k: &str| std::env::var_os(k).map(|v| v.to_string_lossy().into_owned());
        ScanContext {
            passwd_home: None,
            euid: None,
            home_env: var("HOME"),
            declared_home: None,
            claude_config_dir: var("CLAUDE_CONFIG_DIR"),
            codex_home: var("CODEX_HOME"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            extra_paths,
        }
    }
}

// ---------------------------------------------------------------- path helpers

/// macOS volumes are case-insensitive: compare path components without case there only.
fn comp_eq(a: &OsStr, b: &OsStr) -> bool {
    if cfg!(target_os = "macos") {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// Lexical normalisation: `.` dropped, `..` resolved against what precedes it (never above the
/// root), repeated separators collapsed; on macOS the `/System/Volumes/Data` prefix is removed.
pub fn normalize(p: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) | Some(Component::Prefix(_)) => {}
                _ => out.push(c),
            },
            other => out.push(other),
        }
    }
    let mut path: PathBuf = out.iter().map(|c| c.as_os_str()).collect();
    if cfg!(target_os = "macos") {
        if let Ok(rest) = path.strip_prefix("/System/Volumes/Data") {
            path = Path::new("/").join(rest);
        }
    }
    path
}

/// The components of `path` below `root`: `Some` (possibly empty) when `path` is `root` or below.
fn rel_below(path: &Path, root: &Path) -> Option<Vec<OsString>> {
    let mut p = path.components();
    for r in root.components() {
        let c = p.next()?;
        if !comp_eq(c.as_os_str(), r.as_os_str()) {
            return None;
        }
    }
    Some(p.map(|c| c.as_os_str().to_os_string()).collect())
}

fn same_path(a: &Path, b: &Path) -> bool {
    matches!(rel_below(&normalize(a), &normalize(b)), Some(r) if r.is_empty())
        && matches!(rel_below(&normalize(b), &normalize(a)), Some(r) if r.is_empty())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    if same_path(a, b) {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => same_path(&x, &y),
        _ => false,
    }
}

// ---------------------------------------------------------------- validation

fn bad(what: &str, why: &str) -> Error {
    Error::Protect(format!("{what} {why}"))
}

/// Validate one override or configured path. `name` is the variable or key, for the message.
/// Empty, relative (and, unless `allow_tilde`, a literal `~`), `/` and the home itself are
/// refused: they would protect nothing useful or everything, and are never silently dropped.
/// With `allow_tilde` a leading `~/` is expanded to `home`.
pub fn validate_path(
    name: &str,
    value: &str,
    home: Option<&Path>,
    allow_tilde: bool,
) -> Result<PathBuf> {
    if value.is_empty() {
        return Err(bad(name, "is empty"));
    }
    if value == "~" || (value.starts_with('~') && !(allow_tilde && value.starts_with("~/"))) {
        return Err(bad(
            name,
            &format!("is {value:?}, a literal ~ that nothing expanded: give an absolute path"),
        ));
    }
    let expanded: PathBuf = if let Some(rest) = value.strip_prefix("~/") {
        match home {
            Some(h) => h.join(rest),
            None => return Err(bad(name, "uses ~/ but the home directory is not known")),
        }
    } else {
        PathBuf::from(value)
    };
    if !expanded.is_absolute() {
        return Err(bad(
            name,
            &format!("is {value:?}, a relative path: give an absolute path"),
        ));
    }
    let n = normalize(&expanded);
    if n.parent().is_none() {
        return Err(bad(name, "is /, which would protect everything"));
    }
    if let Some(h) = home {
        if same_dir(&n, h) {
            return Err(bad(
                name,
                "is the home directory itself, which would protect everything",
            ));
        }
    }
    Ok(n)
}

/// Check a config `protected_paths` entry as far as it can be checked without the home:
/// non-empty, absolute or `~/`-relative, not `/`, not `~`. (Home-equal is refused by the scan.)
pub fn validate_config_path(value: &str) -> Result<()> {
    if value == "~/" || value == "~" {
        return Err(bad(
            "protected_paths entry",
            &format!("{value:?} is the home directory itself, which would protect everything"),
        ));
    }
    validate_path(
        "protected_paths entry",
        value,
        Some(Path::new("/home/placeholder")),
        true,
    )
    .map(|_| ())
}

// ---------------------------------------------------------------- scan side

fn lossy(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn make_root(spelled: PathBuf, code: HoldCode, carve: CarveLayout, source: &str) -> ProtectedRoot {
    let (canonical, exists, unreadable) = match std::fs::canonicalize(&spelled) {
        Ok(c) => (Some(lossy(&normalize(&c))), true, false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, false, false),
        // Permission denied or anything else: it exists as far as we can tell, and its real path
        // is unknown, so it is compared by its spelled path and nodes that cannot be resolved are
        // held.
        Err(_) => (None, true, true),
    };
    ProtectedRoot {
        spelled: lossy(&spelled),
        canonical,
        exists,
        unreadable,
        code,
        carve,
        source: source.to_string(),
        bytes: 0,
    }
}

/// Check the process context and resolve the protected and cloud-sync roots. Refuses euid 0, a
/// `HOME` that is not the passwd home (or `--home`), and any bad override.
pub fn resolve(ctx: &ScanContext) -> Result<ProtectContext> {
    if ctx.euid == Some(0) {
        return Err(Error::Protect(
            "refusing to scan as root (euid 0): under sudo the home and the agent homes it \
             protects are not yours; run as your own user"
                .into(),
        ));
    }
    let passwd = ctx.passwd_home.as_deref().map(normalize);
    let env_home = match ctx.home_env.as_deref() {
        Some("") => return Err(bad("HOME", "is empty")),
        Some(h) => {
            let p = PathBuf::from(h);
            if !p.is_absolute() {
                return Err(bad("HOME", &format!("is {h:?}, a relative path")));
            }
            Some(normalize(&p))
        }
        None => None,
    };
    let declared = ctx.declared_home.as_deref().map(normalize);
    let primary: PathBuf = match (&declared, &passwd, &env_home) {
        (Some(d), _, Some(e)) if same_dir(d, e) => d.clone(),
        (Some(d), _, _) => {
            return Err(Error::Protect(format!(
                "--home {} must be the same directory as HOME ({}): set HOME to it too",
                d.display(),
                env_home
                    .as_deref()
                    .map_or("unset".to_string(), |h| h.display().to_string())
            )))
        }
        (None, Some(p), Some(e)) if same_dir(p, e) => p.clone(),
        (None, Some(p), Some(e)) => {
            return Err(Error::Protect(format!(
                "HOME ({}) differs from the passwd home ({}): refusing, because the agent homes \
                 to protect are resolved under the real home; unset HOME or fix it \
                 (use --home DIR with HOME=DIR for a disposable fixture home)",
                e.display(),
                p.display()
            )))
        }
        (None, Some(p), None) => p.clone(),
        (None, None, Some(e)) => e.clone(),
        (None, None, None) => {
            return Err(Error::Protect(
                "cannot find the home directory (HOME is unset and no passwd entry was given)"
                    .into(),
            ))
        }
    };
    let mut homes: Vec<(PathBuf, &str)> = vec![(primary.clone(), "built-in")];
    if let Some(p) = &passwd {
        if !same_dir(p, &primary) {
            homes.push((p.clone(), "built-in (passwd home)"));
        }
    }

    let mut roots: Vec<ProtectedRoot> = Vec::new();
    let push = |r: ProtectedRoot, roots: &mut Vec<ProtectedRoot>| {
        if !roots.iter().any(|x| x.spelled == r.spelled) {
            roots.push(r);
        }
    };
    for (home, source) in &homes {
        for (rel, carve) in BUILTIN_PROTECTED {
            push(
                make_root(home.join(rel), HoldCode::Protected, carve, source),
                &mut roots,
            );
        }
        for rel in SYNCED_FIXED {
            push(
                make_root(home.join(rel), HoldCode::Synced, CarveLayout::None, source),
                &mut roots,
            );
        }
        if let Ok(rd) = std::fs::read_dir(home) {
            let mut names: Vec<OsString> = rd.flatten().map(|e| e.file_name()).collect();
            names.sort();
            for name in names {
                let lower = name.to_string_lossy().to_lowercase();
                if SYNCED_PREFIXES
                    .iter()
                    .any(|p| lower.starts_with(&p.to_lowercase()))
                {
                    push(
                        make_root(
                            home.join(&name),
                            HoldCode::Synced,
                            CarveLayout::None,
                            source,
                        ),
                        &mut roots,
                    );
                }
            }
        }
    }
    let mut over = |name: &str, value: &Option<String>, sub: Option<&str>, carve: CarveLayout| {
        if let Some(v) = value {
            let mut p = validate_path(name, v, Some(&primary), false)?;
            if let Some(s) = sub {
                p = p.join(s);
            }
            push(make_root(p, HoldCode::Protected, carve, name), &mut roots);
        }
        Ok::<(), Error>(())
    };
    over(
        "CLAUDE_CONFIG_DIR",
        &ctx.claude_config_dir,
        None,
        CarveLayout::Worktrees,
    )?;
    over("CODEX_HOME", &ctx.codex_home, None, CarveLayout::Worktrees)?;
    over(
        "XDG_CONFIG_HOME",
        &ctx.xdg_config_home,
        Some("claude"),
        CarveLayout::None,
    )?;
    for extra in &ctx.extra_paths {
        let p = validate_path("protected_paths entry", extra, Some(&primary), true)?;
        push(
            make_root(p, HoldCode::Protected, CarveLayout::None, "config"),
            &mut roots,
        );
    }
    Ok(ProtectContext {
        home: lossy(&primary),
        passwd_home: passwd.as_deref().map(lossy),
        uid: ctx.euid,
        home_env: ctx.home_env.clone(),
        claude_config_dir: ctx.claude_config_dir.clone(),
        codex_home: ctx.codex_home.clone(),
        xdg_config_home: ctx.xdg_config_home.clone(),
        roots,
    })
}

/// The canonical path of a node as recorded in the scan: `(real_path, unreadable)`.
pub fn node_real_path(path: &Path) -> (Option<String>, bool) {
    match std::fs::canonicalize(path) {
        Ok(c) => (Some(lossy(&normalize(&c))), false),
        // Any failure, not only a permission error, means the node cannot be shown to be
        // outside protected data: it is held.
        Err(_) => (None, true),
    }
}

/// Add the walk's bytes to each root: `dirs` pairs a directory's spelled path and its own
/// allocated bytes; `real_of` gives the directory's real path (root-prefix swap). A directory
/// counts toward a root when either of its paths is at or below either of the root's.
pub fn attribute_bytes<'a>(
    roots: &mut [ProtectedRoot],
    dirs: impl Iterator<Item = (&'a Path, u64)>,
    real_of: &dyn Fn(&Path) -> PathBuf,
) {
    let ids: Vec<Vec<PathBuf>> = roots.iter().map(root_ids).collect();
    for (dir, bytes) in dirs {
        if bytes == 0 {
            continue;
        }
        let lex = normalize(dir);
        let real = normalize(&real_of(dir));
        for (i, r) in ids.iter().enumerate() {
            if r.iter()
                .any(|rp| rel_below(&lex, rp).is_some() || rel_below(&real, rp).is_some())
            {
                roots[i].bytes = roots[i].bytes.saturating_add(bytes);
            }
        }
    }
}

// ---------------------------------------------------------------- plan side

/// Why a node is held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub code: HoldCode,
    /// The root (or package directory) responsible; `(unreadable)` for an unresolvable node.
    pub root: String,
}

/// The protection verdict for one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing protects it.
    Free,
    /// Kept whatever its facts.
    Hold(Hold),
    /// A checkout in a carve-out position inside a protected root: it may get only
    /// `backup-then-remove`; any other removing action becomes keep.
    CarveOut(String),
}

fn root_ids(r: &ProtectedRoot) -> Vec<PathBuf> {
    if !r.exists {
        return Vec::new();
    }
    let mut v = vec![normalize(Path::new(&r.spelled))];
    if let Some(c) = &r.canonical {
        let c = normalize(Path::new(c));
        if !v.iter().any(|x| same_path(x, &c)) {
            v.push(c);
        }
    }
    v
}

fn node_ids(n: &Node) -> Vec<PathBuf> {
    let mut v = vec![normalize(Path::new(&n.path))];
    if let Some(r) = &n.real_path {
        let r = normalize(Path::new(r));
        if !v.iter().any(|x| same_path(x, &r)) {
            v.push(r);
        }
    }
    v
}

fn is_git_checkout(c: Class) -> bool {
    matches!(c, Class::Repo | Class::Worktree)
}

/// The package directory above `p` (a proper ancestor whose name ends with a package suffix).
fn package_above(p: &Path) -> Option<PathBuf> {
    let comps: Vec<Component> = p.components().collect();
    let mut acc = PathBuf::new();
    for (i, c) in comps.iter().enumerate() {
        acc.push(c.as_os_str());
        if i + 1 == comps.len() {
            break;
        }
        if let Component::Normal(name) = c {
            let lower = name.to_string_lossy().to_lowercase();
            if PACKAGE_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
                return Some(acc);
            }
        }
    }
    None
}

fn carve_position(
    layout: CarveLayout,
    rel: &[OsString],
    root: &Path,
    git_paths: &dyn Fn(&Path) -> bool,
) -> bool {
    let first = |name: &str| rel.first().is_some_and(|c| comp_eq(c, OsStr::new(name)));
    match layout {
        CarveLayout::None => false,
        CarveLayout::Worktrees => rel.len() == 2 && first("worktrees"),
        CarveLayout::AgentHarness => {
            (rel.len() == 3 && first("work"))
                || (rel.len() == 4
                    && first("work")
                    && !git_paths(&root.join(&rel[0]).join(&rel[1]).join(&rel[2])))
        }
    }
}

/// Decide, for every node, whether it is held. Pure: only the scan is read.
pub fn judge(scan: &ScanResult) -> Vec<Verdict> {
    let n = scan.nodes.len();
    let roots: Vec<(&ProtectedRoot, Vec<PathBuf>)> = scan
        .protect
        .roots
        .iter()
        .map(|r| (r, root_ids(r)))
        .collect();
    let ids: Vec<Vec<PathBuf>> = scan.nodes.iter().map(node_ids).collect();
    // Checkout paths, for "the task directory is itself a checkout".
    let git_ids: Vec<&PathBuf> = scan
        .nodes
        .iter()
        .zip(&ids)
        .filter(|(nd, _)| is_git_checkout(nd.class) || nd.class == Class::BareRepo)
        .flat_map(|(_, v)| v.iter())
        .collect();
    let is_git_path = |p: &Path| git_ids.iter().any(|g| same_path(g, p));

    let raw = |i: usize, exempt_protected_inside: bool| -> Verdict {
        let node = &scan.nodes[i];
        if node.real_unreadable {
            return Verdict::Hold(Hold {
                code: HoldCode::Protected,
                root: "(unreadable)".into(),
            });
        }
        let my = &ids[i];
        let mut inside_protected: Vec<&ProtectedRoot> = Vec::new();
        let mut all_carve = true;
        let mut contained_protected: Option<&ProtectedRoot> = None;
        let mut synced: Option<&ProtectedRoot> = None;
        for (r, rids) in &roots {
            let mut inside = false;
            let mut contains = false;
            let mut inside_pairs = 0usize;
            let mut carve_pairs = 0usize;
            for np in my {
                for rp in rids {
                    if let Some(rel) = rel_below(np, rp) {
                        inside = true;
                        inside_pairs += 1;
                        if r.code == HoldCode::Protected
                            && !rel.is_empty()
                            && carve_position(r.carve, &rel, rp, &is_git_path)
                        {
                            carve_pairs += 1;
                        }
                    } else if rel_below(rp, np).is_some() {
                        contains = true;
                    }
                }
            }
            match r.code {
                HoldCode::Protected => {
                    if inside {
                        inside_protected.push(r);
                        // Every identity pair (lexical or canonical path of the node against
                        // either path of the root) that puts the node inside must be a carve
                        // position; one that is not makes it a plain hold.
                        if carve_pairs != inside_pairs {
                            all_carve = false;
                        }
                    } else if contains && contained_protected.is_none() {
                        contained_protected = Some(r);
                    }
                }
                HoldCode::Synced => {
                    if (inside || contains) && synced.is_none() {
                        synced = Some(r);
                    }
                }
                HoldCode::Package => {}
            }
        }
        let mut carve_root: Option<String> = None;
        if !inside_protected.is_empty() && !exempt_protected_inside {
            if all_carve && is_git_checkout(node.class) {
                carve_root = Some(inside_protected[0].spelled.clone());
            } else {
                return Verdict::Hold(Hold {
                    code: HoldCode::Protected,
                    root: inside_protected[0].spelled.clone(),
                });
            }
        }
        if let Some(r) = contained_protected {
            return Verdict::Hold(Hold {
                code: HoldCode::Protected,
                root: r.spelled.clone(),
            });
        }
        if let Some(r) = synced {
            return Verdict::Hold(Hold {
                code: HoldCode::Synced,
                root: r.spelled.clone(),
            });
        }
        for p in my {
            if let Some(pkg) = package_above(p) {
                return Verdict::Hold(Hold {
                    code: HoldCode::Package,
                    root: lossy(&pkg),
                });
            }
        }
        match carve_root {
            Some(r) => Verdict::CarveOut(r),
            None => Verdict::Free,
        }
    };

    let mut verdicts: Vec<Verdict> = (0..n).map(|i| raw(i, false)).collect();
    // Regenerable and cache directories inside a checkout that is in a carve-out position keep
    // their normal action: removing them takes nothing but what the project's tooling rebuilds.
    for i in 0..n {
        let node = &scan.nodes[i];
        if !node.class.is_artifact() {
            continue;
        }
        if !matches!(&verdicts[i], Verdict::Hold(h) if h.code == HoldCode::Protected && h.root != "(unreadable)")
        {
            continue;
        }
        // Nearest checkout above (bounded walk: the parent links may be corrupt).
        let mut cur = node.parent;
        let mut steps = 0;
        let mut anc = None;
        while let Some(c) = cur {
            if c >= n || steps > n {
                break;
            }
            if scan.nodes[c].class.is_git() {
                anc = Some(c);
                break;
            }
            cur = scan.nodes[c].parent;
            steps += 1;
        }
        if let Some(a) = anc {
            if matches!(verdicts[a], Verdict::CarveOut(_)) {
                verdicts[i] = raw(i, true);
            }
        }
    }
    verdicts
}

/// Refuse a scan the plan will not trust for protection: one without protected roots (older
/// schema), or one made by another user than the process planning from it (`euid`).
pub fn check_plan_scan(scan: &ScanResult, euid: Option<u32>) -> Result<()> {
    if scan.protect.roots.is_empty() {
        return Err(Error::Protect(
            "the scan has no protected_roots (it was written by an older version or by hand): \
             run the scan again with this version"
                .into(),
        ));
    }
    if let Some(e) = euid {
        match scan.protect.uid {
            Some(u) if u == e => {}
            Some(u) => {
                return Err(Error::Protect(format!(
                    "the scan was made by uid {u} but this process runs as uid {e}: protected \
                     roots belong to the user who scanned; scan again as this user"
                )))
            }
            None => {
                return Err(Error::Protect(
                    "the scan does not record the user that made it, so its protected roots \
                     cannot be tied to this user: scan again with this version"
                        .into(),
                ))
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
