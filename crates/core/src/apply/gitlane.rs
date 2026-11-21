//! The git lane (spec C1-C5, C7, I-5, R-T).
//!
//! A git piece is a repository's work tree plus its `.git` directory. Apply:
//!
//! 1. (prepare, also in a dry run, read-only) re-reads the repository: `.git` must be a plain
//!    directory with no alternates; credentials stored in its config or admin files keep it
//!    (R-T) unless `--secrets upload`; every work-tree file raw-identical to its HEAD blob is
//!    recorded by object id (the object store is backed up with the piece, I-1), every other file
//!    and every file of `.git` is uploaded through the backup lane (C3, C7);
//! 2. (act only, before any upload) when the plan names a remote and apply establishes that it is
//!    private (C5: listed in `private_remotes`, or `gh` reports it private), pushes recovery refs
//!    without force from a quarantine repository that borrows the objects (the repository's own
//!    config, hooks and index are never used), then confirms every ref with `ls-remote`. Public
//!    or unknown visibility is the fallback: the git-directory backup (C1, C7). A push or
//!    confirmation that fails on a remote proven private keeps the piece (F6).
//!
//! Git always runs with system and global config replaced (`GIT_CONFIG_NOSYSTEM`, an isolated
//! `GIT_CONFIG_GLOBAL` holding only the user's `credential.*` keys), no hooks, no prompts, no lazy
//! fetch, and a protocol allowlist. Remote URLs are compared and printed with userinfo removed.

use super::input::{Inputs, PieceKind, PlanPiece, Recovery};
use super::manifest::Row;
use super::names;
use super::recheck::{Enumerated, Member};
use super::run::{Content, PMember, Prepared};
use super::{fsops, ApplyOptions};
use crate::redact::{diag, strip_url_credentials};
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Deadline for one network git or gh command.
const NET_TIMEOUT: Duration = Duration::from_secs(300);
/// Deadline for one local git command.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(600);
/// Most unpushed commits of one branch whose trees are checked for secret names; a branch with
/// more is not pushed (it stays in the git-directory backup).
const MAX_UNPUSHED: usize = 500;

/// Repo-local config keys allowed for a push (I-5 F1). Any other key means no push.
fn key_allowed(key: &str) -> bool {
    const EXACT: [&str; 8] = [
        "core.repositoryformatversion",
        "core.filemode",
        "core.bare",
        "core.logallrefupdates",
        "core.ignorecase",
        "core.precomposeunicode",
        "core.symlinks",
        "extensions.objectformat",
    ];
    let k = key.to_ascii_lowercase();
    if EXACT.contains(&k.as_str()) || k == "user.name" || k == "user.email" {
        return true;
    }
    let (first, last) = match (k.find('.'), k.rfind('.')) {
        (Some(a), Some(b)) if a < b => (&k[..a], &k[b + 1..]),
        _ => return false,
    };
    matches!(
        (first, last),
        ("remote", "url")
            | ("remote", "fetch")
            | ("branch", "remote")
            | ("branch", "merge")
            | ("branch", "rebase")
    )
}

/// How the recovery refs will be pushed, decided at prepare (read-only).
#[derive(Debug, Clone)]
pub(crate) enum PushPlan {
    /// No push: the git-directory backup is the recovery (C7). The reason says why.
    Fallback(String),
    /// Push to `url` (no userinfo) once privacy holds.
    Push { url: String, privacy: Privacy },
}

/// How privacy is established (C5).
#[derive(Debug, Clone)]
pub(crate) enum Privacy {
    /// The URL is listed in `private_remotes`.
    Declared,
    /// `gh api repos/<owner>/<repo>` must report it private (act only; a dry run runs no gh).
    Gh { owner_repo: String },
}

/// One file or link for the WIP tree.
#[derive(Debug, Clone)]
pub(crate) struct WipEntry {
    pub rel: String,
    /// `100644`, `100755` or `120000`.
    pub mode: &'static str,
    /// The baseline of the member (a file is re-read and must still match it).
    pub id: fsops::Ident,
    /// A link's target bytes.
    pub link_target: Option<String>,
}

/// What the act-only recovery step needs.
#[derive(Debug, Clone)]
pub(crate) struct GitWork {
    pub git_dir: PathBuf,
    pub head: Option<String>,
    /// HEAD's branch (short name), `None` when detached.
    pub head_branch: Option<String>,
    /// Local branches (short name, oid).
    pub branches: Vec<(String, String)>,
    pub push: PushPlan,
    pub wip_add: Vec<WipEntry>,
    pub wip_del: Vec<String>,
}

/// The git environment and flags (I-5). `global` is the generated global config, or none
/// (`/dev/null`) for local read-only commands.
pub(crate) fn git_command(
    opts: &ApplyOptions,
    git_dir: &Path,
    global: Option<&Path>,
    hooks_dir: &Path,
) -> Command {
    let mut c = Command::new(&opts.git);
    for k in [
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_CEILING_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_SSH",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "GIT_PROXY_COMMAND",
        "GIT_EXTERNAL_DIFF",
        "GIT_TRACE",
        "GIT_TRACE_PACKET",
        "GIT_TRACE_CURL",
        "GIT_CURL_VERBOSE",
    ] {
        c.env_remove(k);
    }
    c.env("GIT_DIR", git_dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            global.map_or_else(|| PathBuf::from("/dev/null"), Path::to_path_buf),
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes")
        .env("GIT_PROTOCOL_FROM_USER", "0")
        .env("LC_ALL", "C");
    let file_allow = if opts.file_remotes_are_network() {
        "always"
    } else {
        "never"
    };
    let hooks = format!("core.hooksPath={}", hooks_dir.display());
    for kv in [
        hooks.as_str(),
        "core.fsmonitor=false",
        "core.untrackedCache=false",
        "gc.auto=0",
        "maintenance.auto=false",
        "protocol.allow=never",
        "protocol.https.allow=always",
        "protocol.ssh.allow=always",
        "protocol.ext.allow=never",
        "http.followRedirects=false",
        "commit.gpgSign=false",
        "push.gpgSign=false",
        "push.recurseSubmodules=no",
        "submodule.recurse=false",
    ] {
        c.arg("-c").arg(kv);
    }
    c.arg("-c").arg(format!("protocol.file.allow={file_allow}"));
    c
}

/// Run a command with a deadline (the process group is killed on expiry). stdout is returned;
/// stderr goes into the error, redacted.
pub(crate) fn run_cmd(
    cmd: Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    what: &str,
) -> Result<Vec<u8>, String> {
    run_cmd_ok(cmd, input, timeout, what, &[0])
}

/// [`run_cmd`] accepting any exit code in `ok` (for example `check-ignore`, which exits 1 when
/// nothing is ignored).
pub(crate) fn run_cmd_ok(
    mut cmd: Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    what: &str,
    ok: &[i32],
) -> Result<Vec<u8>, String> {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{what}: cannot start: {e}"))?;
    let feeder = input.map(|bytes| {
        let mut stdin = child.stdin.take().expect("piped");
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        })
    });
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    let t_out = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.read_to_end(&mut v);
        v
    });
    let t_err = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err.read_to_end(&mut v);
        v
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if start.elapsed() > timeout => {
                if let Some(p) = rustix::process::Pid::from_raw(child.id() as i32) {
                    let _ = rustix::process::kill_process_group(p, rustix::process::Signal::KILL);
                }
                let _ = child.wait();
                return Err(format!("{what}: timed out"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("{what}: {e}")),
        }
    };
    if let Some(f) = feeder {
        let _ = f.join();
    }
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();
    if !status.code().is_some_and(|c| ok.contains(&c)) {
        let msg = String::from_utf8_lossy(&stderr);
        let first: String = msg.lines().take(3).collect::<Vec<_>>().join(" / ");
        return Err(format!("{what} failed: {}", diag(first.trim())));
    }
    Ok(stdout)
}

/// `core.hooksPath`: a path that cannot exist (below a character device), so no hook is ever
/// found and nothing is written for it (a dry run writes nothing).
fn empty_hooks_dir(_opts: &ApplyOptions) -> PathBuf {
    PathBuf::from("/dev/null/disk-cleaner-no-hooks")
}

/// Read-only git on the repository itself (only after its config passed the allowlist).
fn src_git(opts: &ApplyOptions, gd: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut c = git_command(opts, gd, None, &empty_hooks_dir(opts));
    c.args(args);
    run_cmd(c, None, LOCAL_TIMEOUT, &format!("git {}", args[0]))
}

/// The entries of one config file (`git config --file F --list -z`, includes not followed).
fn config_entries(opts: &ApplyOptions, file: &Path) -> Result<Vec<(String, String)>, String> {
    let mut c = Command::new(&opts.git);
    c.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("LC_ALL", "C")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .arg("config")
        .arg("--file")
        .arg(file)
        .args(["--no-includes", "--list", "-z"]);
    let out = run_cmd(c, None, LOCAL_TIMEOUT, "git config")?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|e| !e.is_empty())
        .map(|e| {
            let s = String::from_utf8_lossy(e);
            match s.split_once('\n') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (s.to_string(), String::new()),
            }
        })
        .collect())
}

/// Does `text` hold a URL with userinfo that is a credential (R-T)? An ssh user name alone is
/// not one.
fn has_url_credential(text: &str) -> bool {
    text.split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .filter(|t| t.contains("://"))
        .any(|t| {
            let t = t.trim_end_matches([',', ';', ')']);
            let rest = &t[t.find("://").map_or(0, |i| i + 3)..];
            // Userinfo runs to the last '@' before the path (a password may hold '/').
            let Some(at) = rest.rfind('@') else {
                return false;
            };
            let user = &rest[..at];
            // A password, or a user name that is itself a token (`https://ghp_x@host/r`). A plain
            // user name (`https://alice@bitbucket.org/...`) is not a credential.
            user.contains(':') || looks_like_token(user)
        })
}

/// A user name that is a token: a known token prefix, or a long run of letters and digits.
fn looks_like_token(user: &str) -> bool {
    let u = user.to_ascii_lowercase();
    const PREFIXES: [&str; 8] = [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "x-token-auth",
    ];
    if PREFIXES.iter().any(|p| u.starts_with(p)) || u.contains('%') {
        return true;
    }
    let alnum = user.chars().filter(|c| c.is_ascii_alphanumeric()).count();
    user.len() >= 20 && alnum * 10 >= user.len() * 9 && user.chars().any(|c| c.is_ascii_digit())
}

/// A credential stored in the repository's config or small admin files (R-T, N2). Returns what
/// was found, never its value.
fn stored_credential(gd: &Path, entries: &[(String, String)]) -> Option<String> {
    for (k, v) in entries {
        let lk = k.to_ascii_lowercase();
        if lk.starts_with("credential.") {
            return Some(format!(
                "{} in .git/config",
                crate::redact::config_key_label(k)
            ));
        }
        if lk.starts_with("http.") && lk.ends_with(".extraheader") {
            return Some(format!(
                "{} in .git/config",
                crate::redact::config_key_label(k)
            ));
        }
        if has_url_credential(v) {
            return Some(format!(
                "a URL with a user name or token in {}",
                crate::redact::config_key_label(k)
            ));
        }
    }
    let mut files = vec![gd.join("FETCH_HEAD"), gd.join("config.worktree")];
    for sub in ["remotes", "branches"] {
        if let Ok(rd) = std::fs::read_dir(gd.join(sub)) {
            files.extend(rd.flatten().map(|e| e.path()));
        }
    }
    for f in files {
        let Ok(md) = f.symlink_metadata() else {
            continue;
        };
        if !md.is_file() || md.len() > 4 << 20 {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&f) {
            if has_url_credential(&String::from_utf8_lossy(&bytes)) {
                let name = f
                    .strip_prefix(gd)
                    .map(|r| r.to_string_lossy().to_string())
                    .unwrap_or_default();
                return Some(format!(
                    "a URL with a user name or token in .git/{}",
                    diag(&name)
                ));
            }
        }
    }
    None
}

/// The URL forms that may be pushed to, with their host.
#[derive(Debug, PartialEq, Eq)]
enum UrlKind {
    Https(String),
    Ssh(String),
    File,
}

/// Classify a (credential-free) URL; `None` for anything not pushable (I-5 M1).
fn classify_url(url: &str) -> Option<UrlKind> {
    if url.starts_with('-') || url.contains("::") || url.chars().any(|c| c.is_control()) {
        return None;
    }
    let host_of = |rest: &str| -> String {
        let hostport = rest.split('/').next().unwrap_or("");
        let host = hostport.rsplit('@').next().unwrap_or("");
        host.split(':').next().unwrap_or("").to_ascii_lowercase()
    };
    if let Some(rest) = url.strip_prefix("https://") {
        return Some(UrlKind::Https(host_of(rest)));
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        return Some(UrlKind::Ssh(host_of(rest)));
    }
    if url.starts_with("file:///") {
        return Some(UrlKind::File);
    }
    if url.contains("://") {
        return None;
    }
    // scp-like user@host:path (a colon before any slash).
    let colon = url.find(':')?;
    if url[..colon].contains('/') || colon == 0 {
        return None;
    }
    let host = url[..colon].rsplit('@').next().unwrap_or("");
    if host.is_empty() {
        return None;
    }
    Some(UrlKind::Ssh(host.to_ascii_lowercase()))
}

/// The form two URLs are compared in: no userinfo credential, no trailing `/` or `.git`.
pub(crate) fn url_key(url: &str) -> String {
    let s = strip_url_credentials(url.trim());
    let s = s.trim_end_matches('/');
    s.strip_suffix(".git").unwrap_or(s).to_string()
}

/// `owner/repo` of a github.com URL.
fn github_repo(url: &str) -> Option<String> {
    let k = url_key(url);
    let path = if let Some(r) = k.strip_prefix("https://") {
        r.split_once('/')?.1.to_string()
    } else if let Some(r) = k.strip_prefix("ssh://") {
        r.split_once('/')?.1.to_string()
    } else {
        k.split_once(':')?.1.to_string()
    };
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() != 2 {
        return None;
    }
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    };
    (ok(parts[0]) && ok(parts[1])).then(|| format!("{}/{}", parts[0], parts[1]))
}

/// git's object id of a blob with these bytes (SHA-1 object format).
fn blob_oid(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", bytes.len()).as_bytes());
    h.update(bytes);
    hex::encode(h.finalize())
}

/// sha256 and git blob id of a member, read through a directory handle; the identity must hold
/// before and after.
fn hash_file_git(root: &Path, m: &Member) -> Result<(String, String, fsops::Ident), String> {
    let r = fsops::open_dir_abs(root).map_err(|e| format!("{}: {e}", root.display()))?;
    let (mut f, id) = fsops::open_file_rel(&r, &m.rel).map_err(|e| format!("{}: {e}", m.rel))?;
    if !id.same_strict(&m.id) {
        return Err(format!("{} changed after it was listed", m.rel));
    }
    let mut s = Sha256::new();
    let mut g = Sha1::new();
    g.update(format!("blob {}\0", id.len).as_bytes());
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("{}: {e}", m.rel))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        s.update(&buf[..n]);
        g.update(&buf[..n]);
    }
    let after = rustix::fs::fstat(&f).map_err(|e| e.to_string())?;
    if !fsops::ident_of(&after).same_strict(&id) || total != id.len {
        return Err(format!("{} changed while it was read", m.rel));
    }
    Ok((hex::encode(s.finalize()), hex::encode(g.finalize()), id))
}

fn under(rel: &str, prefix: &str) -> bool {
    rel == prefix || rel.starts_with(&format!("{prefix}/"))
}

fn in_git_dir(rel: &str) -> bool {
    under(rel, ".git")
}

/// One HEAD tree entry.
struct TreeEntry {
    mode: String,
    kind: String,
    oid: String,
}

fn parse_ls_tree(out: &[u8]) -> BTreeMap<String, TreeEntry> {
    let mut m = BTreeMap::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let s = String::from_utf8_lossy(rec);
        let Some((meta, path)) = s.split_once('\t') else {
            continue;
        };
        let f: Vec<&str> = meta.split(' ').collect();
        if f.len() != 3 {
            continue;
        }
        m.insert(
            path.to_string(),
            TreeEntry {
                mode: f[0].to_string(),
                kind: f[1].to_string(),
                oid: f[2].to_string(),
            },
        );
    }
    m
}

fn lines(out: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(out)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Prepare a git piece (read-only; runs in a dry run too).
pub(crate) fn prepare(
    opts: &ApplyOptions,
    _inputs: &Inputs,
    p: &PlanPiece,
    en: &Enumerated,
) -> Result<Prepared, String> {
    if p.kind != PieceKind::GitNode {
        return Err("a git piece must be a git-node".into());
    }
    let git = p.git.as_ref().ok_or("a git piece without its git block")?;
    let gd = p.path.join(".git");
    let md = gd
        .symlink_metadata()
        .map_err(|_| "its .git is missing".to_string())?;
    if !md.is_dir() {
        return Err("its .git is not a plain directory (linked worktree or submodule)".into());
    }
    if gd
        .join("objects/info/alternates")
        .symlink_metadata()
        .is_ok()
    {
        return Err("its object store borrows objects from another repository (alternates)".into());
    }
    if !en.members.iter().any(|m| m.rel == ".git/HEAD") {
        return Err("its .git/HEAD is not a member of the piece".into());
    }
    let entries = config_entries(opts, &gd.join("config"))?;
    if let Some(what) = stored_credential(&gd, &entries) {
        if !opts.secrets_upload {
            return Err(format!(
                "credential stored in repo config ({what}); kept (upload it only with --secrets upload)"
            ));
        }
    }
    let bad_key = entries
        .iter()
        .find(|(k, _)| !key_allowed(k))
        .map(|(k, _)| crate::redact::config_key_label(k));
    let sha1_format = entries
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("extensions.objectformat"))
        .is_none_or(|(_, v)| v.eq_ignore_ascii_case("sha1"));
    // An excluded path or a symlink inside .git means the backed-up git directory may not hold
    // every object: then nothing restores from it (every file is uploaded raw).
    let git_partial = p.excludes.iter().any(|e| in_git_dir(e))
        || en
            .members
            .iter()
            .any(|m| in_git_dir(&m.rel) && m.id.kind == 'l');
    // Plumbing on the repository itself only after its config passed the allowlist.
    let trusted = bad_key.is_none() && sha1_format;
    let mut head = None;
    let mut head_branch = None;
    let mut branches = Vec::new();
    let mut tree = BTreeMap::new();
    if trusted {
        head = src_git(opts, &gd, &["rev-parse", "--verify", "-q", "HEAD^{commit}"])
            .ok()
            .and_then(|o| lines(&o).into_iter().next());
        head_branch = src_git(opts, &gd, &["symbolic-ref", "-q", "HEAD"])
            .ok()
            .and_then(|o| lines(&o).into_iter().next())
            .and_then(|r| r.strip_prefix("refs/heads/").map(str::to_string));
        let refs = src_git(
            opts,
            &gd,
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/heads",
            ],
        )?;
        for l in lines(&refs) {
            if let Some((oid, r)) = l.split_once(' ') {
                if let Some(b) = r.strip_prefix("refs/heads/") {
                    branches.push((b.to_string(), oid.to_string()));
                }
            }
        }
        if let Some(h) = &head {
            tree = parse_ls_tree(&src_git(
                opts,
                &gd,
                &["ls-tree", "-r", "-z", "--full-tree", h.as_str()],
            )?);
        }
    }

    // Classify the members.
    let mut members = Vec::with_capacity(en.members.len());
    let mut candidates: Vec<usize> = Vec::new();
    let mut wip_add = Vec::new();
    let mut untracked_wip: Vec<usize> = Vec::new();
    for m in &en.members {
        let wt = !in_git_dir(&m.rel);
        match m.id.kind {
            'f' => {
                let (sha, oid, id) = hash_file_git(&p.path, m)?;
                let te = tree.get(&m.rel);
                let exec = id.mode & 0o111 != 0;
                let same_head = wt
                    && trusted
                    && !git_partial
                    && te.is_some_and(|t| {
                        t.kind == "blob"
                            && t.oid == oid
                            && (t.mode == "100755") == exec
                            && (t.mode == "100644" || t.mode == "100755")
                    });
                if same_head {
                    candidates.push(members.len());
                    members.push(PMember {
                        rel: m.rel.clone(),
                        id,
                        content: Content::GitBlob {
                            oid,
                            sha256: sha,
                            git_dir: gd.clone(),
                        },
                    });
                    continue;
                }
                if wt && !names::rel_is_secret(&m.rel) && id.len <= opts.wip_max_file_bytes {
                    if te.is_none() {
                        untracked_wip.push(wip_add.len());
                    }
                    wip_add.push(WipEntry {
                        rel: m.rel.clone(),
                        mode: if exec { "100755" } else { "100644" },
                        id,
                        link_target: None,
                    });
                }
                members.push(PMember {
                    rel: m.rel.clone(),
                    id,
                    content: Content::Blob { sha256: sha },
                });
            }
            'l' => {
                let r = fsops::open_dir_abs(&p.path).map_err(|e| e.to_string())?;
                let t = fsops::readlink_rel(&r, &m.rel).map_err(|e| format!("{}: {e}", m.rel))?;
                let target = String::from_utf8(t)
                    .map_err(|_| format!("{}: link target is not UTF-8", m.rel))?;
                if wt && !names::rel_is_secret(&m.rel) {
                    let te = tree.get(&m.rel);
                    let same = te.is_some_and(|t| {
                        t.mode == "120000" && t.oid == blob_oid(target.as_bytes())
                    });
                    if !same {
                        if te.is_none() {
                            untracked_wip.push(wip_add.len());
                        }
                        wip_add.push(WipEntry {
                            rel: m.rel.clone(),
                            mode: "120000",
                            id: m.id,
                            link_target: Some(target.clone()),
                        });
                    }
                }
                members.push(PMember {
                    rel: m.rel.clone(),
                    id: m.id,
                    content: Content::Link { target },
                });
            }
            _ => return Err(format!("{} is not a regular file or symlink", m.rel)),
        }
    }
    // Every HEAD-identical blob must be present in the object store being backed up
    // (Finding 2): a missing one (partial clone) is uploaded raw.
    if !candidates.is_empty() {
        let mut input = Vec::new();
        for &k in &candidates {
            if let Content::GitBlob { oid, .. } = &members[k].content {
                input.extend_from_slice(oid.as_bytes());
                input.push(b'\n');
            }
        }
        let mut c = git_command(opts, &gd, None, &empty_hooks_dir(opts));
        c.args(["cat-file", "--batch-check=%(objectname) %(objecttype)"]);
        let out = run_cmd(c, Some(input), LOCAL_TIMEOUT, "git cat-file")?;
        let present: BTreeSet<String> = lines(&out)
            .into_iter()
            .filter_map(|l| {
                let (o, t) = l.split_once(' ')?;
                (t == "blob").then(|| o.to_string())
            })
            .collect();
        for &k in &candidates {
            if let Content::GitBlob { oid, sha256, .. } = &members[k].content {
                if !present.contains(oid) {
                    members[k].content = Content::Blob {
                        sha256: sha256.clone(),
                    };
                }
            }
        }
    }

    // Recovery decision (read-only).
    let push = match &git.recovery {
        Recovery::None => PushPlan::Fallback("no unsaved work to recover".into()),
        Recovery::BundleToBackup => PushPlan::Fallback(
            "no remote can take recovery refs; the git directory backup holds every commit".into(),
        ),
        Recovery::PushRecoveryRefs { remote, url, .. } => {
            push_plan(opts, &entries, bad_key.as_deref(), sha1_format, remote, url)
        }
    };
    // Untracked files that are ignored never enter the WIP (M2): only when a push may happen.
    if matches!(push, PushPlan::Push { .. }) && !untracked_wip.is_empty() {
        let mut input = Vec::new();
        for &k in &untracked_wip {
            input.extend_from_slice(wip_add[k].rel.as_bytes());
            input.push(0);
        }
        let mut c = git_command(opts, &gd, None, &empty_hooks_dir(opts));
        c.env("GIT_WORK_TREE", &p.path)
            .args(["check-ignore", "--no-index", "-z", "--stdin"]);
        // Exit 1 means nothing is ignored.
        let ignored: BTreeSet<String> =
            run_cmd_ok(c, Some(input), LOCAL_TIMEOUT, "git check-ignore", &[0, 1])?
                .split(|b| *b == 0)
                .filter(|r| !r.is_empty())
                .map(|r| String::from_utf8_lossy(r).to_string())
                .collect();
        wip_add.retain(|w| !ignored.contains(&w.rel));
    }
    let member_set: BTreeSet<&str> = en.members.iter().map(|m| m.rel.as_str()).collect();
    let wip_del: Vec<String> = tree
        .iter()
        .filter(|(path, t)| t.kind == "blob" && !member_set.contains(path.as_str()))
        .filter(|(path, _)| !p.excludes.iter().any(|e| under(path, e)))
        .filter(|(path, _)| p.path.join(path).symlink_metadata().is_err())
        .map(|(path, _)| path.clone())
        .collect();

    let gitblobs = members
        .iter()
        .filter(|m| matches!(m.content, Content::GitBlob { .. }))
        .count();
    let reason = match &push {
        PushPlan::Fallback(why) => format!(
            "recovery: the git directory backup ({why}); {gitblobs} unchanged files restore from it"
        ),
        PushPlan::Push { url, privacy } => format!(
            "recovery refs to {url} ({}), then the git directory backup; {gitblobs} unchanged files restore from it",
            match privacy {
                Privacy::Declared => "listed in private_remotes".to_string(),
                Privacy::Gh { .. } => "pushed only if gh reports it private at apply time".to_string(),
            }
        ),
    };
    Ok(Prepared {
        piece: p.id,
        members,
        dirs: en.dirs.clone(),
        remove_root: true,
        extra_rows: Vec::new(),
        restore: "disk-cleaner restore --path <path> (the work tree and its .git)".into(),
        reason,
        git: Some(Box::new(GitWork {
            git_dir: gd,
            head,
            head_branch,
            branches,
            push,
            wip_add,
            wip_del,
        })),
    })
}

fn push_plan(
    opts: &ApplyOptions,
    entries: &[(String, String)],
    bad_key: Option<&str>,
    sha1_format: bool,
    remote: &str,
    plan_url: &str,
) -> PushPlan {
    if let Some(k) = bad_key {
        return PushPlan::Fallback(format!(
            "its config has {k}, which apply does not trust for a push"
        ));
    }
    if !sha1_format {
        return PushPlan::Fallback("its object format is not SHA-1".into());
    }
    let key = format!("remote.{remote}.url");
    let live: Vec<&String> = entries
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(&key))
        .map(|(_, v)| v)
        .collect();
    let [live] = live.as_slice() else {
        return PushPlan::Fallback(format!("remote {} has no single URL", diag(remote)));
    };
    let url = strip_url_credentials(live);
    if url_key(&url) != url_key(plan_url) {
        return PushPlan::Fallback("the remote's URL changed since the plan".into());
    }
    let allowed = match classify_url(&url) {
        Some(UrlKind::Https(h)) | Some(UrlKind::Ssh(h)) => {
            opts.git_hosts.iter().any(|g| g.eq_ignore_ascii_case(&h))
        }
        Some(UrlKind::File) => opts.file_remotes_are_network(),
        None => false,
    };
    if !allowed {
        return PushPlan::Fallback(format!("{} is not a remote on an allowed host", diag(&url)));
    }
    let declared = opts
        .private_remotes
        .iter()
        .any(|d| url_key(d) == url_key(&url));
    if declared {
        return PushPlan::Push {
            url,
            privacy: Privacy::Declared,
        };
    }
    let is_github = matches!(classify_url(&url), Some(UrlKind::Https(h)) | Some(UrlKind::Ssh(h)) if h == "github.com");
    match (is_github, github_repo(&url)) {
        (true, Some(owner_repo)) => PushPlan::Push {
            url,
            privacy: Privacy::Gh { owner_repo },
        },
        _ => PushPlan::Fallback(format!(
            "{} is not known to be private (list it in private_remotes to allow a push)",
            diag(&url)
        )),
    }
}

/// What the recovery step did.
pub(crate) struct Recovered {
    /// The confirmed refs, to record before any removal.
    pub row: Option<Row>,
    /// A human summary for the report.
    pub note: String,
}

/// `gh api repos/<owner_repo>` reports it private (C5): `private: true`, `visibility: private`
/// and the same `full_name`. Anything else, or an error, is not private.
fn gh_private(opts: &ApplyOptions, owner_repo: &str) -> Result<(), String> {
    let mut c = Command::new(&opts.gh);
    for k in [
        "GH_HOST",
        "GH_REPO",
        "GH_ENTERPRISE_TOKEN",
        "GITHUB_ENTERPRISE_TOKEN",
    ] {
        c.env_remove(k);
    }
    c.env("GH_PROMPT_DISABLED", "1").env("NO_COLOR", "1").args([
        "api",
        "--hostname",
        "github.com",
        &format!("repos/{owner_repo}"),
    ]);
    let out = run_cmd(c, None, NET_TIMEOUT, "gh api").map_err(|e| format!("gh: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_slice(&out).map_err(|_| "gh returned something that is not JSON")?;
    let private = v.get("private").and_then(|x| x.as_bool()) == Some(true);
    let vis = v.get("visibility").and_then(|x| x.as_str()) == Some("private");
    let name = v
        .get("full_name")
        .and_then(|x| x.as_str())
        .is_some_and(|n| n.eq_ignore_ascii_case(owner_repo));
    if private && vis && name {
        Ok(())
    } else {
        Err("gh does not report it private".into())
    }
}

/// The isolated global config: only the user's `credential.*` keys (N3), from the system config
/// and then the global one (git's own order), so a helper such as osxkeychain set in either is
/// used. Nothing else from those files (`insteadOf`, `include`, `sshCommand`) is loaded.
fn isolated_global(opts: &ApplyOptions, dir: &Path) -> Result<PathBuf, String> {
    write_credential_config(
        &opts.git,
        opts.home.as_deref(),
        None,
        &dir.join("global.gitconfig"),
    )
}

/// Write `path` (0600, new) with the `credential.*` keys of the system config (`system`, or
/// git's default) and of the global config under `home`.
pub(crate) fn write_credential_config(
    git: &Path,
    home: Option<&Path>,
    system: Option<&Path>,
    path: &Path,
) -> Result<PathBuf, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("cannot create the isolated git config: {e}"))?;
    for scope in ["--system", "--global"] {
        let mut c = Command::new(git);
        if let Some(h) = home {
            c.env("HOME", h).env_remove("XDG_CONFIG_HOME");
        }
        c.env_remove("GIT_CONFIG_NOSYSTEM")
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT");
        match system {
            Some(f) => c.env("GIT_CONFIG_SYSTEM", f),
            None => c.env_remove("GIT_CONFIG_SYSTEM"),
        };
        c.args(["config", scope, "--null", "--get-regexp", "^credential\\."]);
        // Exit 1: no such keys. A missing system file is not an error either.
        let out = run_cmd_ok(c, None, LOCAL_TIMEOUT, "git config", &[0, 1]).unwrap_or_default();
        for e in out.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let s = String::from_utf8_lossy(e);
            let Some((k, v)) = s.split_once('\n') else {
                continue;
            };
            let mut a = Command::new(git);
            a.env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .arg("config")
                .arg("--file")
                .arg(path)
                .arg("--add")
                .arg(k)
                .arg(v);
            run_cmd(a, None, LOCAL_TIMEOUT, "git config --file")?;
        }
    }
    Ok(path.to_path_buf())
}

fn ls_remote(
    opts: &ApplyOptions,
    q: &Path,
    global: &Path,
    url: &str,
) -> Result<BTreeMap<String, String>, String> {
    let mut c = git_command(opts, q, Some(global), &empty_hooks_dir(opts));
    c.args(["ls-remote", "--end-of-options", url]);
    let out = run_cmd(c, None, NET_TIMEOUT, "git ls-remote")?;
    Ok(lines(&out)
        .into_iter()
        .filter_map(|l| {
            let (oid, r) = l.split_once('\t')?;
            Some((r.to_string(), oid.to_string()))
        })
        .collect())
}

/// git 2.32 or later (I-5 F2): older git ignores `GIT_CONFIG_GLOBAL` and would load the user's
/// global config (an `insteadOf` rewrite could redirect the push).
fn git_version_ok(opts: &ApplyOptions) -> Result<(), String> {
    let mut c = Command::new(&opts.git);
    c.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("--version");
    let out = run_cmd(c, None, LOCAL_TIMEOUT, "git --version")?;
    let text = String::from_utf8_lossy(&out);
    let ver = text
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or("");
    let mut it = ver.split('.').map(|n| {
        n.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(0)
    });
    let (major, minor) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    if (major, minor) >= (2, 32) {
        Ok(())
    } else {
        Err(format!(
            "git {} is older than 2.32, which apply needs to push in isolation",
            diag(ver)
        ))
    }
}

/// Recovery (act only): push recovery refs to a remote proven private, confirm them, and return
/// the row to record. `Err` keeps the piece (F6).
pub(crate) fn recover(
    opts: &ApplyOptions,
    run: &str,
    p: &PlanPiece,
    w: &GitWork,
) -> Result<Recovered, String> {
    let (url, privacy) = match &w.push {
        PushPlan::Fallback(why) => {
            return Ok(Recovered {
                row: None,
                note: format!("recovery: the git directory backup ({why})"),
            })
        }
        PushPlan::Push { url, privacy } => (url.clone(), privacy),
    };
    if let Privacy::Gh { owner_repo } = privacy {
        if let Err(why) = gh_private(opts, owner_repo) {
            return Ok(Recovered {
                row: None,
                note: format!(
                    "recovery: the git directory backup ({} is not verified private: {why})",
                    diag(&url)
                ),
            });
        }
    }
    if let Err(why) = git_version_ok(opts) {
        return Ok(Recovered {
            row: None,
            note: format!("recovery: the git directory backup ({why})"),
        });
    }
    let work = opts.state_dir.join(format!("git-{run}-{}", p.id));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("cannot create the git scratch: {e}"))?;
    let res = recover_in(opts, p, w, &url, &work);
    let _ = std::fs::remove_dir_all(&work);
    res
}

fn recover_in(
    opts: &ApplyOptions,
    p: &PlanPiece,
    w: &GitWork,
    url: &str,
    work: &Path,
) -> Result<Recovered, String> {
    let hooks = empty_hooks_dir(opts);
    let global = isolated_global(opts, work)?;
    // The quarantine repository: our own bare repo that borrows the objects (read-only).
    let q = work.join("q.git");
    let mut c = Command::new(&opts.git);
    c.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args(["init", "--bare", "-q", "--template="])
        .arg(&q);
    run_cmd(c, None, LOCAL_TIMEOUT, "git init")?;
    let objects = w.git_dir.join("objects");
    std::fs::write(
        q.join("objects/info/alternates"),
        format!("{}\n", objects.display()),
    )
    .map_err(|e| format!("cannot write the quarantine alternates: {e}"))?;
    let qgit = |args: &[&str], input: Option<Vec<u8>>| -> Result<Vec<u8>, String> {
        let mut c = git_command(opts, &q, Some(&global), &hooks);
        c.args(args);
        run_cmd(c, input, LOCAL_TIMEOUT, &format!("git {}", args[0]))
    };

    // A fresh view of the remote (PR-stage).
    let before = ls_remote(opts, &q, &global, url)?;
    let mut probe = Vec::new();
    for oid in before.values() {
        probe.extend_from_slice(oid.as_bytes());
        probe.push(b'\n');
    }
    let known: Vec<String> = if probe.is_empty() {
        Vec::new()
    } else {
        lines(&qgit(
            &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
            Some(probe),
        )?)
        .into_iter()
        .filter_map(|l| {
            let (o, t) = l.split_once(' ')?;
            (t == "commit").then(|| o.to_string())
        })
        .collect()
    };
    let mut notes: Vec<String> = Vec::new();
    // Commits of `tip` not on the remote; Err(reason) when the branch must not be pushed (M2).
    let unpushed = |tip: &str| -> Result<Vec<String>, String> {
        let mut input = format!("{tip}\n");
        for k in &known {
            input.push_str(&format!("^{k}\n"));
        }
        let commits = lines(&qgit(
            &[
                "rev-list",
                "--stdin",
                &format!("--max-count={}", MAX_UNPUSHED + 1),
            ],
            Some(input.into_bytes()),
        )?);
        if commits.len() > MAX_UNPUSHED {
            return Err(format!("more than {MAX_UNPUSHED} unpushed commits"));
        }
        for cm in &commits {
            let names = qgit(&["ls-tree", "-r", "-z", "--name-only", cm.as_str()], None)?;
            if let Some(s) = names
                .split(|b| *b == 0)
                .filter(|r| !r.is_empty())
                .map(|r| String::from_utf8_lossy(r).to_string())
                .find(|r| names::rel_is_secret(r))
            {
                return Err(format!(
                    "an unpushed commit holds the secret-named {}",
                    diag(&s)
                ));
            }
        }
        Ok(commits)
    };
    let date = utc_date(opts.now);
    let label = &opts.label;
    let mut want: Vec<(String, String)> = Vec::new();
    let mut head_ok = true;
    for (b, oid) in &w.branches {
        match unpushed(oid) {
            Ok(c) if c.is_empty() => {}
            Ok(_) => want.push((format!("refs/recovery/{label}/{date}/{b}"), oid.clone())),
            Err(why) => {
                if w.head_branch.as_deref() == Some(b.as_str()) {
                    head_ok = false;
                }
                notes.push(format!("branch {} not pushed: {why}", diag(b)));
            }
        }
    }
    let mut detached_unpushed = false;
    if let (Some(head), true) = (&w.head, head_ok) {
        if w.head_branch.is_none() {
            match unpushed(head) {
                Ok(c) => detached_unpushed = !c.is_empty(),
                Err(why) => {
                    head_ok = false;
                    notes.push(format!("detached HEAD not pushed: {why}"));
                }
            }
        }
    }
    // The WIP commit: HEAD plus modified and untracked files (no secrets, no ignored, no large).
    if let (Some(head), true) = (&w.head, head_ok) {
        if !w.wip_add.is_empty() || !w.wip_del.is_empty() {
            let index = work.join("wip-index");
            let qidx = |args: &[&str], input: Option<Vec<u8>>| -> Result<Vec<u8>, String> {
                let mut c = git_command(opts, &q, Some(&global), &hooks);
                c.env("GIT_INDEX_FILE", &index).args(args);
                run_cmd(c, input, LOCAL_TIMEOUT, &format!("git {}", args[0]))
            };
            qidx(&["read-tree", head.as_str()], None)?;
            let mut info = String::new();
            let root = fsops::open_dir_abs(&p.path).map_err(|e| e.to_string())?;
            for e in &w.wip_add {
                let bytes = match &e.link_target {
                    Some(t) => t.clone().into_bytes(),
                    None => {
                        let (mut f, id) = fsops::open_file_rel(&root, &e.rel)
                            .map_err(|x| format!("{}: {x}", e.rel))?;
                        if !id.same_strict(&e.id) {
                            return Err(format!("{} changed before the recovery", e.rel));
                        }
                        let mut v = Vec::with_capacity(id.len as usize);
                        f.read_to_end(&mut v).map_err(|x| x.to_string())?;
                        v
                    }
                };
                let oid = lines(&qgit(
                    &["hash-object", "-w", "--no-filters", "--stdin"],
                    Some(bytes),
                )?)
                .into_iter()
                .next()
                .ok_or("git hash-object printed nothing")?;
                info.push_str(&format!("{} {oid}\t{}\0", e.mode, e.rel));
            }
            for d in &w.wip_del {
                info.push_str(&format!("0 {}\t{d}\0", "0".repeat(40)));
            }
            qidx(
                &["update-index", "-z", "--index-info"],
                Some(info.into_bytes()),
            )?;
            // C2: no secret-named path in the WIP tree, also one inherited from HEAD (its history
            // stays in the `.git` backup, never in a new pushed tree).
            let secret_paths = |names: &[u8]| -> Vec<String> {
                names
                    .split(|b| *b == 0)
                    .filter(|r| !r.is_empty())
                    .map(|r| String::from_utf8_lossy(r).to_string())
                    .filter(|r| names::rel_is_secret(r))
                    .collect()
            };
            let inherited = secret_paths(&qidx(&["ls-files", "-z", "--cached"], None)?);
            if !inherited.is_empty() {
                // Mode 0 removes an entry; it needs no work tree (the quarantine repo is bare).
                let mut del = String::new();
                for r in &inherited {
                    del.push_str(&format!("0 {}\t{r}\0", "0".repeat(40)));
                }
                qidx(
                    &["update-index", "-z", "--index-info"],
                    Some(del.into_bytes()),
                )?;
            }
            let tree = lines(&qidx(&["write-tree"], None)?)
                .into_iter()
                .next()
                .ok_or("git write-tree printed nothing")?;
            let left = secret_paths(&qgit(
                &["ls-tree", "-r", "-z", "--name-only", tree.as_str()],
                None,
            )?);
            if let Some(s) = left.first() {
                return Err(format!(
                    "the recovery tree would hold the secret-named {}",
                    diag(s)
                ));
            }
            let mut c = git_command(opts, &q, Some(&global), &hooks);
            let when = format!("{} +0000", opts.now);
            c.env("GIT_AUTHOR_NAME", "disk-cleaner")
                .env("GIT_AUTHOR_EMAIL", "disk-cleaner@localhost")
                .env("GIT_COMMITTER_NAME", "disk-cleaner")
                .env("GIT_COMMITTER_EMAIL", "disk-cleaner@localhost")
                .env("GIT_AUTHOR_DATE", &when)
                .env("GIT_COMMITTER_DATE", &when)
                .args(["commit-tree", tree.as_str(), "-p", head.as_str(), "-m"])
                .arg("disk-cleaner: work in progress before removal");
            let wip = lines(&run_cmd(c, None, LOCAL_TIMEOUT, "git commit-tree")?)
                .into_iter()
                .next()
                .ok_or("git commit-tree printed nothing")?;
            let name = w.head_branch.clone().unwrap_or_else(|| "HEAD".into());
            want.push((format!("refs/recovery/{label}/{date}/wip/{name}"), wip));
        } else if detached_unpushed {
            // A detached HEAD with commits on no branch and no remote, and a clean tree.
            want.push((
                format!("refs/recovery/{label}/{date}/wip/HEAD"),
                head.clone(),
            ));
        }
    }
    // Never forced (C4): a ref already there with another oid is skipped.
    let mut push: Vec<(String, String)> = Vec::new();
    let mut confirmed: Vec<(String, String)> = Vec::new();
    for (r, oid) in want {
        match before.get(&r) {
            Some(o) if *o == oid => confirmed.push((r, oid)),
            Some(_) => notes.push(format!(
                "{} already exists on the remote with another commit; not forced",
                diag(&r)
            )),
            None => push.push((r, oid)),
        }
    }
    if !push.is_empty() {
        let mut c = git_command(opts, &q, Some(&global), &hooks);
        c.args([
            "push",
            "--no-verify",
            "--porcelain",
            "--end-of-options",
            url,
        ]);
        for (r, oid) in &push {
            c.arg(format!("{oid}:{r}"));
        }
        run_cmd(c, None, NET_TIMEOUT, "git push")?;
        let after = ls_remote(opts, &q, &global, url)?;
        for (r, oid) in &push {
            if after.get(r) != Some(oid) {
                return Err(format!(
                    "{} is not on the remote at the pushed commit after the push",
                    diag(r)
                ));
            }
        }
        confirmed.extend(push);
    }
    let note = if confirmed.is_empty() {
        format!(
            "recovery: nothing unsaved to push{}; the git directory is backed up",
            if notes.is_empty() {
                String::new()
            } else {
                format!(" ({})", notes.join("; "))
            }
        )
    } else {
        format!(
            "recovery refs confirmed on {}: {}{}; the git directory is backed up",
            diag(url),
            confirmed
                .iter()
                .map(|(r, _)| diag(r))
                .collect::<Vec<_>>()
                .join(", "),
            if notes.is_empty() {
                String::new()
            } else {
                format!(" ({})", notes.join("; "))
            }
        )
    };
    let row = (!confirmed.is_empty()).then(|| Row::Recovery {
        plan_piece: p.id,
        path: p.path.clone(),
        url: url.to_string(),
        refs: confirmed,
    });
    Ok(Recovered { row, note })
}

/// `YYYY-MM-DD` (UTC) of a unix time.
pub fn utc_date(t: i64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = t.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Raw bytes of a blob from a git directory, without filters.
pub(crate) fn cat_blob(git: &Path, git_dir: &Path, oid: &str) -> Result<Vec<u8>, String> {
    let out = std::process::Command::new(git)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_DIR", git_dir)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args(["cat-file", "blob", oid])
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git cat-file failed for {oid}"));
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_classification() {
        assert_eq!(
            classify_url("https://github.com/o/r.git"),
            Some(UrlKind::Https("github.com".into()))
        );
        assert_eq!(
            classify_url("git@github.com:o/r.git"),
            Some(UrlKind::Ssh("github.com".into()))
        );
        assert_eq!(
            classify_url("ssh://git@gitlab.com:22/o/r"),
            Some(UrlKind::Ssh("gitlab.com".into()))
        );
        assert_eq!(classify_url("file:///tmp/x.git"), Some(UrlKind::File));
        for bad in [
            "-uhost:x",
            "ext::sh -c x",
            "http://github.com/o/r",
            "git://github.com/o/r",
            "/local/path",
            "./rel",
            "fd::3",
        ] {
            assert_eq!(classify_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn url_keys_and_repos() {
        assert_eq!(
            url_key("https://u:tok@github.com/o/r.git/"),
            "https://github.com/o/r"
        );
        assert_eq!(url_key("git@github.com:o/r.git"), "git@github.com:o/r");
        assert_eq!(
            github_repo("git@github.com:o/r.git").as_deref(),
            Some("o/r")
        );
        assert_eq!(
            github_repo("https://github.com/o/r").as_deref(),
            Some("o/r")
        );
        assert_eq!(github_repo("https://github.com/o/r/x"), None);
    }

    #[test]
    fn url_credentials_are_detected() {
        assert!(has_url_credential("url = https://user:CANARY@host/r"));
        assert!(has_url_credential("https://ghp_token@github.com/o/r"));
        assert!(!has_url_credential("url = ssh://git@github.com/o/r"));
        assert!(!has_url_credential("url = git@github.com:o/r.git"));
        assert!(!has_url_credential("url = https://github.com/o/r"));
        assert!(!has_url_credential(
            "url = https://alice@bitbucket.org/team/r.git"
        ));
        assert!(has_url_credential(
            "https://github_pat_11ABCDEF@github.com/o/r"
        ));
        assert!(has_url_credential(
            "https://a1b2c3d4e5f6a7b8c9d0e1f2@host/r"
        ));
        assert!(has_url_credential("https://x-token-auth@bitbucket.org/r"));
    }

    #[test]
    fn allowlist() {
        assert!(key_allowed("core.bare"));
        assert!(key_allowed("remote.origin.url"));
        assert!(key_allowed("branch.main.merge"));
        assert!(key_allowed("remote.a.b.c.fetch"));
        for bad in [
            "core.hookspath",
            "core.sshcommand",
            "include.path",
            "remote.origin.pushurl",
            "url.x.insteadof",
            "core.fsmonitor",
            "credential.helper",
        ] {
            assert!(!key_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn credential_helpers_from_system_and_global_only() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".gitconfig"),
            "[credential]\n\thelper = store\n[url \"file:///decoy\"]\n\tinsteadOf = https://github.com/\n[core]\n\tsshCommand = evil\n",
        )
        .unwrap();
        let system = t.path().join("system.gitconfig");
        std::fs::write(
            &system,
            "[credential]\n\thelper = osxkeychain\n[include]\n\tpath = /nonexistent\n",
        )
        .unwrap();
        let out = t.path().join("iso.gitconfig");
        write_credential_config(Path::new("git"), Some(&home), Some(&system), &out).unwrap();
        let mut c = Command::new("git");
        c.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["config", "--file"])
            .arg(&out)
            .args(["--list"]);
        let listed = String::from_utf8(run_cmd(c, None, LOCAL_TIMEOUT, "list").unwrap()).unwrap();
        assert_eq!(
            listed.lines().collect::<Vec<_>>(),
            vec!["credential.helper=osxkeychain", "credential.helper=store"],
            "{listed}"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&out).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn dates() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400), "2000-02-29");
        assert_eq!(utc_date(1_795_229_183), "2026-11-21");
    }

    #[test]
    fn blob_ids_match_git() {
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(
            blob_oid(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }
}
