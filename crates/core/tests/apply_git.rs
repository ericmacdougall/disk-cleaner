//! Apply: the git lane (spec AC-5, AC-6, NEG-5, NEG-6, NEG-14, NEG-15, R-T). Every remote is a
//! local `file://` bare repository, treated as a network remote only through the `test-hooks`
//! seam; nothing here contacts a hosting service (a github.com URL only ever reaches the fake
//! `gh` and the git wrapper, which refuses every network command).
#![cfg(unix)]

mod apply_common;

use apply_common::*;
use disk_cleaner_core::apply::restore::{restore, RestoreOptions};
use disk_cleaner_core::apply::{apply, ApplyError, ApplyOptions, ApplyReport, CrashPoint};
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

/// Fixture git: no system or global config.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "protocol.file.allow=always",
        ])
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Repo {
    dir: PathBuf,
    bare: PathBuf,
    url: String,
}

const UNCHANGED: &[u8] = b"UNCHANGED-CANARY-7f3a: this tracked file never changes\n";

/// A repository with: a pushed commit, an unpushed commit, a stash, a modified tracked file, an
/// untracked file, an ignored 1 KB file, an untracked file above the WIP limit (2 MiB, limit
/// 1 MiB in these tests) and an ignored `.env`.
fn repo(fx: &Fx) -> Repo {
    let base = fx.tmp.path().canonicalize().unwrap();
    let bare = base.join("bare.git");
    fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "-b", "main"]);
    let dir = fx.data.join("repo");
    fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    fx.write("repo/README.md", b"hello\n");
    fx.write("repo/src/lib.rs", UNCHANGED);
    fx.write("repo/tracked.txt", b"v1\n");
    fx.write("repo/gone.txt", b"deleted in the work tree\n");
    fx.write("repo/.gitignore", b"*.log\n.env\n");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "A"]);
    let url = format!("file://{}", bare.display());
    git(&dir, &["remote", "add", "origin", &url]);
    git(&dir, &["push", "-q", "-u", "origin", "main"]);
    fx.write("repo/notes.md", b"unpushed work\n");
    git(&dir, &["add", "notes.md"]);
    git(&dir, &["commit", "-q", "-m", "B (unpushed)"]);
    fx.write("repo/tracked.txt", b"stashed change\n");
    git(&dir, &["stash", "-q"]);
    fx.write("repo/tracked.txt", b"v2-MODIFIED\n");
    fx.write("repo/new.txt", b"UNTRACKED\n");
    fx.write("repo/build.log", &[b'l'; 1024]);
    fx.write("repo/big.bin", &vec![9u8; 2 << 20]);
    fx.write("repo/.env", b"SECRET=ENV-CANARY-91c2\n");
    fs::remove_file(dir.join("gone.txt")).unwrap();
    // NEG-16: an untracked symlink to a file outside the repository.
    let canary = base.join("outside-canary.txt");
    fs::write(&canary, b"LINK-CANARY-0b7e outside the piece\n").unwrap();
    std::os::unix::fs::symlink(&canary, dir.join("link-out")).unwrap();
    Repo { dir, bare, url }
}

fn push_block(url: &str) -> Value {
    json!({"action": "recover-backup-then-remove", "preserve": true, "excludes_secret": [".env"],
           "recovery": {"method": "push-recovery-refs", "remote": "origin", "url": url,
                        "fallback": "bundle-to-backup", "decided_at": "apply"}})
}

fn bundle_block() -> Value {
    json!({"action": "backup-then-remove", "preserve": true, "excludes_secret": [".env"],
           "recovery": {"method": "bundle-to-backup"}})
}

/// A git wrapper that logs every invocation; with `offline`, every network command fails.
fn git_wrapper(fx: &Fx, offline: bool) -> PathBuf {
    git_wrapper_with(fx, offline, "")
}

/// [`git_wrapper`] with extra bash run before delegating (it may `exit`).
fn git_wrapper_with(fx: &Fx, offline: bool, extra: &str) -> PathBuf {
    let real = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let w = fx.fake_dir.join("git");
    let block = if offline {
        "case \" $* \" in *\" push \"*|*\" ls-remote \"*|*\" fetch \"*) echo 'fatal: offline test wrapper' >&2; exit 128;; esac\n"
    } else {
        ""
    };
    fs::write(
        &w,
        format!(
            "#!/bin/bash\necho \"$@\" >> {d}/git.log\n{block}{extra}\nexec {real} \"$@\"\n",
            d = fx.fake_dir.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&w, fs::Permissions::from_mode(0o755)).unwrap();
    w
}

fn git_log(fx: &Fx) -> String {
    fs::read_to_string(fx.fake_dir.join("git.log")).unwrap_or_default()
}

fn net_calls(fx: &Fx) -> usize {
    git_log(fx)
        .lines()
        .filter(|l| {
            let w: Vec<&str> = l.split(' ').collect();
            w.contains(&"push") || w.contains(&"ls-remote") || w.contains(&"fetch")
        })
        .count()
}

fn gopts(fx: &Fx, act: bool, offline: bool) -> ApplyOptions {
    let mut o = fx.opts(act);
    o.git = git_wrapper(fx, offline);
    o.hooks.file_remotes_are_network = true;
    o.wip_max_file_bytes = 1 << 20;
    o
}

fn ropts(fx: &Fx) -> RestoreOptions {
    RestoreOptions {
        accept_partial: false,
        remote: fx.remote.to_string_lossy().to_string(),
        rclone: fx.fake.clone(),
        git: "git".into(),
        manifest: fx.manifest(),
        from_remote: false,
        label: "testhost".into(),
        run: None,
        timeout: Duration::from_secs(120),
    }
}

fn outcome(rep: &ApplyReport, id: usize) -> (String, String) {
    let p = rep.pieces.iter().find(|p| p.id == id).unwrap();
    (p.outcome.clone(), p.reason.clone())
}

fn recovery_refs(bare: &Path) -> Vec<(String, String)> {
    git(
        bare,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/recovery",
        ],
    )
    .lines()
    .filter_map(|l| l.split_once(' '))
    .map(|(r, o)| (r.to_string(), o.to_string()))
    .collect()
}

/// Every entry of a directory with its stat identity (for "unchanged up to removal").
fn stat_digest(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd {
            let p = e.unwrap().path();
            let md = p.symlink_metadata().unwrap();
            if md.is_dir() {
                stack.push(p.clone());
            }
            out.push(format!(
                "{} {} {} {} {} {}",
                p.display(),
                md.ino(),
                md.len(),
                md.mtime(),
                md.mtime_nsec(),
                md.ctime_nsec()
            ));
        }
    }
    out.sort();
    out
}

static BEFORE_REMOVE: Mutex<Vec<(PathBuf, Vec<String>)>> = Mutex::new(Vec::new());

fn snapshot_git_dir(piece: &Path) {
    let gd = piece.join(".git");
    if gd.is_dir() {
        BEFORE_REMOVE
            .lock()
            .unwrap()
            .push((piece.to_path_buf(), stat_digest(&gd)));
    }
}

#[test]
fn git_lane_end_to_end() {
    let fx = Fx::new();
    let r = repo(&fx);
    let head = git(&r.dir, &["rev-parse", "HEAD"]);
    let before = tree_digest(&r.dir);
    let gd_before = stat_digest(&r.dir.join(".git"));
    let (scan, plan) = docs(
        &[&fx.data],
        &[P::git(&r.dir, &[".env"], push_block(&r.url))],
    );
    let mut o = gopts(&fx, true, false);
    o.private_remotes = vec![r.url.clone()];
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    o.hooks.before_remove = Some(snapshot_git_dir);
    let rep = apply(&scan, &plan, &o).unwrap();
    let (out, why) = outcome(&rep, 0);
    assert_eq!(out, "removed", "{why}");
    assert!(why.contains("recovery refs confirmed"), "{why}");

    // Recovery refs on the remote at the expected commits.
    let refs = recovery_refs(&r.bare);
    let date = disk_cleaner_core::apply::utc_date(o.now);
    let main_ref = format!("refs/recovery/testhost/{date}/main");
    let wip_ref = format!("refs/recovery/testhost/{date}/wip/main");
    assert!(refs.contains(&(main_ref, head.clone())), "{refs:?}");
    let wip = refs
        .iter()
        .find(|(n, _)| *n == wip_ref)
        .unwrap_or_else(|| panic!("no WIP ref: {refs:?}"))
        .1
        .clone();
    assert_eq!(git(&r.bare, &["rev-parse", &format!("{wip}^")]), head);
    let files = git(&r.bare, &["ls-tree", "-r", "--name-only", &wip]);
    let files: Vec<&str> = files.lines().collect();
    for want in ["new.txt", "tracked.txt", "notes.md", "src/lib.rs"] {
        assert!(
            files.contains(&want),
            "{want} missing from the WIP: {files:?}"
        );
    }
    for never in [".env", "build.log", "big.bin", "gone.txt"] {
        assert!(!files.contains(&never), "{never} must not be in the WIP");
    }
    // The symlink is entered as a link (its target bytes), never followed.
    let link = git(&r.bare, &["ls-tree", &wip, "link-out"]);
    assert!(link.starts_with("120000 blob "), "{link}");
    assert_eq!(
        git(&r.bare, &["cat-file", "-p", &format!("{wip}:link-out")]),
        fx.tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("outside-canary.txt")
            .display()
            .to_string()
    );
    let grep = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-C")
        .arg(&r.bare)
        .args(["grep", "-q", "LINK-CANARY", &wip])
        .status()
        .unwrap();
    assert!(!grep.success(), "the link target's content was pushed");
    assert_eq!(
        git(&r.bare, &["show", &format!("{wip}:tracked.txt")]),
        "v2-MODIFIED"
    );

    // The real .git (index included) was not written up to removal.
    let seen = BEFORE_REMOVE.lock().unwrap().clone();
    let snap = seen
        .iter()
        .find(|(p, _)| *p == r.dir)
        .expect("before_remove saw the repo");
    assert_eq!(snap.1, gd_before, ".git changed before removal");

    // Removed except the secret, which stays.
    let left: Vec<String> = fs::read_dir(&r.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(left, vec![".env".to_string()]);

    // Unchanged tracked files were not uploaded raw (they restore from the object store).
    let shards = fx.all_shard_bytes();
    assert!(!shards.windows(UNCHANGED.len()).any(|w| w == UNCHANGED));
    assert!(!shards.windows(14).any(|w| w == b"ENV-CANARY-91c"));
    assert!(!shards.windows(11).any(|w| w == b"LINK-CANARY"));
    let manifest = fs::read_to_string(fx.manifest()).unwrap();
    assert!(manifest.contains("\"git_oid\""));
    assert!(manifest.contains("\"t\":\"recovery\""));

    // With the remote gone, restore reproduces the work tree and .git from storage alone.
    fs::remove_dir_all(&r.bare).unwrap();
    let rr = restore(&r.dir, &ropts(&fx)).unwrap();
    assert!(rr.failed.is_empty() && rr.conflicts.is_empty(), "{rr:?}");
    assert_eq!(tree_digest(&r.dir), before);
    assert_eq!(git(&r.dir, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&r.dir, &["stash", "list"]).lines().count(), 1);
}

#[test]
fn restore_of_one_unchanged_file_brings_its_git_dir() {
    let fx = Fx::new();
    let r = repo(&fx);
    // Packed refs and objects: refs/heads is then an empty directory.
    git(&r.dir, &["pack-refs", "--all"]);
    git(&r.dir, &["gc", "-q"]);
    let (scan, plan) = docs(&[&fx.data], &[P::git(&r.dir, &[".env"], bundle_block())]);
    let mut o = gopts(&fx, true, true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    assert_eq!(outcome(&rep, 0).0, "removed", "{rep:#?}");
    let target = r.dir.join("src/lib.rs");
    let rr = restore(&target, &ropts(&fx)).unwrap();
    assert!(rr.failed.is_empty(), "{rr:?}");
    assert_eq!(fs::read(&target).unwrap(), UNCHANGED);
    assert!(r.dir.join(".git/HEAD").exists());
}

#[test]
fn dry_run_git_piece_runs_no_network_and_writes_nothing() {
    let fx = Fx::new();
    let r = repo(&fx);
    let before = tree_digest(&r.dir);
    let (scan, plan) = docs(
        &[&fx.data],
        &[P::git(&r.dir, &[".env"], push_block(&r.url))],
    );
    let mut o = gopts(&fx, false, false);
    o.private_remotes = vec![r.url.clone()];
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    let (out, why) = outcome(&rep, 0);
    assert!(out.starts_with("would"), "{out}: {why}");
    assert!(why.contains("recovery refs to"), "{why}");
    assert_eq!(net_calls(&fx), 0, "{}", git_log(&fx));
    assert!(recovery_refs(&r.bare).is_empty());
    assert_eq!(tree_digest(&r.dir), before);
    assert!(!fx.manifest().exists());
    assert!(fs::read_dir(&fx.state).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("git-")));
}

/// A fake gh printing `json` and exiting with `rc`.
fn fake_gh(fx: &Fx, json: &str, rc: i32) -> PathBuf {
    let g = fx.fake_dir.join("gh");
    fs::write(
        &g,
        format!(
            "#!/bin/bash\necho \"$@\" >> {d}/gh.log\ncat <<'EOF'\n{json}\nEOF\nexit {rc}\n",
            d = fx.fake_dir.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&g, fs::Permissions::from_mode(0o755)).unwrap();
    g
}

#[test]
fn no_push_without_verified_privacy() {
    let gh_url = "git@github.com:someone/thing.git";
    let cases: Vec<(&str, String, i32)> = vec![
        (
            "public",
            json!({"private": false, "visibility": "public", "full_name": "someone/thing"})
                .to_string(),
            0,
        ),
        (
            "internal",
            json!({"private": true, "visibility": "internal", "full_name": "someone/thing"})
                .to_string(),
            0,
        ),
        ("404", json!({"message": "Not Found"}).to_string(), 1),
        (
            "missing key",
            json!({"visibility": "private", "full_name": "someone/thing"}).to_string(),
            0,
        ),
        (
            "other name",
            json!({"private": true, "visibility": "private", "full_name": "someone/other"})
                .to_string(),
            0,
        ),
        ("not json", "garbage".to_string(), 0),
    ];
    for (name, body, rc) in cases {
        let fx = Fx::new();
        let r = repo(&fx);
        git(&r.dir, &["remote", "set-url", "origin", gh_url]);
        let before = tree_digest(&r.dir);
        let (scan, plan) = docs(
            &[&fx.data],
            &[P::git(&r.dir, &[".env"], push_block(gh_url))],
        );
        let mut o = gopts(&fx, true, true);
        o.gh = fake_gh(&fx, &body, rc);
        o.oracle = Box::new(agreeing_oracle(&scan, &plan));
        let rep = apply(&scan, &plan, &o).unwrap();
        let (out, why) = outcome(&rep, 0);
        assert_eq!(out, "removed", "{name}: {why}");
        assert!(why.contains("not verified private"), "{name}: {why}");
        assert_eq!(net_calls(&fx), 0, "{name}: {}", git_log(&fx));
        assert!(
            fs::read_to_string(fx.fake_dir.join("gh.log"))
                .unwrap()
                .contains("api --hostname github.com repos/someone/thing"),
            "{name}"
        );
        let rr = restore(&r.dir, &ropts(&fx)).unwrap();
        assert!(rr.failed.is_empty(), "{name}: {rr:?}");
        assert_eq!(tree_digest(&r.dir), before, "{name}");
    }
    // A remote with no privacy evidence at all (not listed, not on github.com): no gh, no push.
    let fx = Fx::new();
    let r = repo(&fx);
    let (scan, plan) = docs(
        &[&fx.data],
        &[P::git(&r.dir, &[".env"], push_block(&r.url))],
    );
    let mut o = gopts(&fx, true, false);
    o.gh = fake_gh(&fx, "{}", 0);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    let (out, why) = outcome(&rep, 0);
    assert_eq!(out, "removed", "{why}");
    assert!(why.contains("not known to be private"), "{why}");
    assert_eq!(net_calls(&fx), 0);
    assert!(!fx.fake_dir.join("gh.log").exists());
    assert!(recovery_refs(&r.bare).is_empty());
    // No remote that can take refs: the bundle route.
    let fx = Fx::new();
    let r = repo(&fx);
    let (scan, plan) = docs(&[&fx.data], &[P::git(&r.dir, &[".env"], bundle_block())]);
    let mut o = gopts(&fx, true, true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    assert_eq!(outcome(&rep, 0).0, "removed");
    assert_eq!(net_calls(&fx), 0);
}

#[test]
fn verified_private_remote_that_fails_keeps_the_repo() {
    // gh reports it private, but the push cannot happen (the wrapper is offline): F6.
    let gh_url = "git@github.com:someone/thing.git";
    let fx = Fx::new();
    let r = repo(&fx);
    git(&r.dir, &["remote", "set-url", "origin", gh_url]);
    let (scan, plan) = docs(
        &[&fx.data],
        &[P::git(&r.dir, &[".env"], push_block(gh_url))],
    );
    let mut o = gopts(&fx, true, true);
    o.gh = fake_gh(
        &fx,
        &json!({"private": true, "visibility": "private", "full_name": "someone/thing"})
            .to_string(),
        0,
    );
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    let (out, why) = outcome(&rep, 0);
    assert_eq!(out, "kept", "{why}");
    assert!(why.contains("recovery failed"), "{why}");
    assert!(r.dir.join(".git/HEAD").exists() && r.dir.join("new.txt").exists());
    assert_eq!(fx.shard_total(), 0, "nothing uploaded for a kept repo");
}

#[test]
fn hostile_config_never_pushes_or_runs_a_hook() {
    let cases: Vec<(&str, Vec<(&str, String)>)> = vec![
        (
            "insteadOf",
            vec![("url.file:///elsewhere.insteadOf", "file:///nowhere".into())],
        ),
        (
            "pushurl",
            vec![("remote.origin.pushurl", "file:///elsewhere.git".into())],
        ),
        (
            "sshCommand",
            vec![("core.sshCommand", "touch SENTINEL".into())],
        ),
        ("hooksPath", vec![("core.hooksPath", "HOOKS".into())]),
        ("include", vec![("include.path", "/nonexistent/x".into())]),
    ];
    for (name, keys) in cases {
        let fx = Fx::new();
        let r = repo(&fx);
        let sentinel = fx.tmp.path().join("SENTINEL");
        let hooks = fx.tmp.path().join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        for d in [&hooks, &r.dir.join(".git/hooks")] {
            for h in ["pre-push", "post-index-change", "reference-transaction"] {
                let f = d.join(h);
                fs::write(
                    &f,
                    format!("#!/bin/sh\necho {h} >> {}\n", sentinel.display()),
                )
                .unwrap();
                fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        for (k, v) in &keys {
            let v = v
                .replace("HOOKS", &hooks.display().to_string())
                .replace("SENTINEL", &sentinel.display().to_string());
            git(&r.dir, &["config", k, &v]);
        }
        let (scan, plan) = docs(
            &[&fx.data],
            &[P::git(&r.dir, &[".env"], push_block(&r.url))],
        );
        let mut o = gopts(&fx, true, false);
        o.private_remotes = vec![r.url.clone()];
        o.oracle = Box::new(agreeing_oracle(&scan, &plan));
        let rep = apply(&scan, &plan, &o).unwrap();
        let (out, why) = outcome(&rep, 0);
        assert_eq!(out, "removed", "{name}: {why}");
        assert!(why.contains("does not trust"), "{name}: {why}");
        assert!(recovery_refs(&r.bare).is_empty(), "{name}");
        assert_eq!(net_calls(&fx), 0, "{name}");
        assert!(!sentinel.exists(), "{name}: a hook ran");
    }
    // Global case: a rewrite in the user's global config is not loaded; the push goes to the
    // literal URL, and the repository's own hooks never run.
    let fx = Fx::new();
    let r = repo(&fx);
    let home = fx.tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let decoy = fx.tmp.path().join("decoy.git");
    fs::create_dir_all(&decoy).unwrap();
    git(&decoy, &["init", "-q", "--bare"]);
    fs::write(
        home.join(".gitconfig"),
        format!(
            "[url \"file://{d}\"]\n\tinsteadOf = {u}\n\tpushInsteadOf = {u}\n[core]\n\thooksPath = /nonexistent\n",
            d = decoy.display(),
            u = r.url
        ),
    )
    .unwrap();
    let sentinel = fx.tmp.path().join("SENTINEL");
    for h in ["pre-push", "post-index-change"] {
        let f = r.dir.join(".git/hooks").join(h);
        fs::write(
            &f,
            format!("#!/bin/sh\necho {h} >> {}\n", sentinel.display()),
        )
        .unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (scan, plan) = docs(
        &[&fx.data],
        &[P::git(&r.dir, &[".env"], push_block(&r.url))],
    );
    let mut o = gopts(&fx, true, false);
    o.home = Some(home);
    o.private_remotes = vec![r.url.clone()];
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    let (out, why) = outcome(&rep, 0);
    assert_eq!(out, "removed", "{why}");
    assert!(
        !recovery_refs(&r.bare).is_empty(),
        "pushed to the literal URL"
    );
    assert!(
        recovery_refs(&decoy).is_empty(),
        "the global rewrite was ignored"
    );
    assert!(!sentinel.exists(), "a hook ran");
}

#[test]
fn unpushed_commit_with_a_secret_is_not_pushed() {
    let fx = Fx::new();
    let r = repo(&fx);
    git(&r.dir, &["add", "-f", ".env"]);
    git(&r.dir, &["commit", "-q", "-m", "adds a secret"]);
    let (scan, plan) = docs(
        &[&fx.data],
        &[P::git(&r.dir, &[".env"], push_block(&r.url))],
    );
    let mut o = gopts(&fx, true, false);
    o.private_remotes = vec![r.url.clone()];
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    let (out, why) = outcome(&rep, 0);
    assert_eq!(out, "removed", "{why}");
    assert!(why.contains("secret-named"), "{why}");
    assert!(
        recovery_refs(&r.bare).is_empty(),
        "{:?}",
        recovery_refs(&r.bare)
    );
    // The secret never left in a push; the bare repo has only what was pushed by the fixture.
    let objs = git(&r.bare, &["rev-list", "--all", "--objects"]);
    assert!(!objs.contains(".env"));
}

#[test]
fn token_in_git_config_keeps_repo() {
    for (name, setup) in [
        ("remote url", 0usize),
        ("FETCH_HEAD", 1),
        ("extraheader", 2),
        ("credential key", 3),
    ] {
        let fx = Fx::new();
        let r = repo(&fx);
        match setup {
            0 => git(
                &r.dir,
                &[
                    "remote",
                    "set-url",
                    "origin",
                    "https://user:TOKEN-CANARY-55@github.com/o/r.git",
                ],
            ),
            1 => {
                fs::write(
                    r.dir.join(".git/FETCH_HEAD"),
                    "0000000000000000000000000000000000000000\t\tbranch 'main' of https://x:TOKEN-CANARY-55@example.com/r\n",
                )
                .unwrap();
                String::new()
            }
            2 => git(
                &r.dir,
                &[
                    "config",
                    "http.https://github.com/.extraheader",
                    "AUTHORIZATION: basic TOKEN-CANARY-55",
                ],
            ),
            _ => git(&r.dir, &["config", "credential.helper", "store"]),
        };
        let (scan, plan) = docs(&[&fx.data], &[P::git(&r.dir, &[".env"], bundle_block())]);
        let mut o = gopts(&fx, true, true);
        o.oracle = Box::new(agreeing_oracle(&scan, &plan));
        let rep = apply(&scan, &plan, &o).unwrap();
        let (out, why) = outcome(&rep, 0);
        assert_eq!(out, "kept", "{name}: {why}");
        assert!(
            why.contains("credential stored in repo config"),
            "{name}: {why}"
        );
        let json = serde_json::to_string(&rep).unwrap();
        assert!(!json.contains("TOKEN-CANARY"), "{name}: {json}");
        let manifest = fs::read_to_string(fx.manifest()).unwrap_or_default();
        assert!(!manifest.contains("TOKEN-CANARY"), "{name}");
        assert!(
            !fx.all_shard_bytes()
                .windows(12)
                .any(|w| w == b"TOKEN-CANARY"),
            "{name}"
        );
        assert!(r.dir.join(".git/config").exists(), "{name}");
    }
}

#[test]
fn new_linked_worktree_after_scan_keeps_repo() {
    let fx = Fx::new();
    let r = repo(&fx);
    let (scan, plan) = docs(&[&fx.data], &[P::git(&r.dir, &[".env"], bundle_block())]);
    fs::create_dir_all(r.dir.join(".git/worktrees/other")).unwrap();
    fs::write(
        r.dir.join(".git/worktrees/other/gitdir"),
        "/elsewhere/.git\n",
    )
    .unwrap();
    let mut o = gopts(&fx, true, true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    let (out, why) = outcome(&rep, 0);
    assert_eq!(out, "kept", "{why}");
    assert!(why.contains("changed since the scan"), "{why}");
}

#[test]
fn unusual_git_dirs_are_kept() {
    // A .git file (linked worktree or submodule).
    let fx = Fx::new();
    let d = fx.data.join("wt");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join(".git"), "gitdir: /elsewhere/.git/worktrees/wt\n").unwrap();
    fs::write(d.join("a.txt"), "a").unwrap();
    let (scan, plan) = docs(&[&fx.data], &[P::git(&d, &[], bundle_block())]);
    let mut o = gopts(&fx, true, true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    let (out, why) = outcome(&rep, 0);
    assert_eq!(out, "kept", "{why}");
    assert!(why.contains("not a plain directory"), "{why}");
    // Alternates.
    let fx = Fx::new();
    let r = repo(&fx);
    fs::write(
        r.dir.join(".git/objects/info/alternates"),
        "/elsewhere/objects\n",
    )
    .unwrap();
    let (scan, plan) = docs(&[&fx.data], &[P::git(&r.dir, &[".env"], bundle_block())]);
    let mut o = gopts(&fx, true, true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    let (out, why) = outcome(&rep, 0);
    assert_eq!(out, "kept", "{why}");
    assert!(why.contains("alternates"), "{why}");
}

#[test]
fn git_piece_without_rederivation_is_kept() {
    let fx = Fx::new();
    let r = repo(&fx);
    let (scan, plan) = docs(&[&fx.data], &[P::git(&r.dir, &[".env"], bundle_block())]);
    // The production oracle re-derives nothing from a fixture scan.
    let rep = apply(&scan, &plan, &gopts(&fx, true, true)).unwrap();
    let (out, why) = outcome(&rep, 0);
    assert_eq!(out, "kept", "{why}");
    assert!(r.dir.join(".git/HEAD").exists());
}

#[test]
fn git_resume_is_idempotent_and_never_pushes_twice() {
    for at in [
        CrashPoint::AfterBlobRows,
        CrashPoint::AfterRemoveBegin,
        CrashPoint::MidUnlink,
    ] {
        let fx = Fx::new();
        let r = repo(&fx);
        let before = tree_digest(&r.dir);
        let (scan, plan) = docs(
            &[&fx.data],
            &[P::git(&r.dir, &[".env"], push_block(&r.url))],
        );
        let mut o = gopts(&fx, true, false);
        o.private_remotes = vec![r.url.clone()];
        o.oracle = Box::new(agreeing_oracle(&scan, &plan));
        o.hooks.crash_at = Some(at);
        match apply(&scan, &plan, &o) {
            Err(ApplyError::InjectedCrash(_)) => {}
            other => panic!("{at:?}: expected the injected crash, got {other:?}"),
        }
        let refs = recovery_refs(&r.bare);
        assert_eq!(refs.len(), 2, "{at:?}: {refs:?}");
        let mut o = gopts(&fx, true, false);
        o.private_remotes = vec![r.url.clone()];
        o.oracle = Box::new(agreeing_oracle(&scan, &plan));
        let rep = apply(&scan, &plan, &o).unwrap();
        let (out, why) = outcome(&rep, 0);
        assert_eq!(out, "removed", "{at:?}: {why}");
        assert_eq!(recovery_refs(&r.bare), refs, "{at:?}: refs unchanged");
        let pushes = git_log(&fx)
            .lines()
            .filter(|l| l.split(' ').any(|w| w == "push"))
            .count();
        assert_eq!(pushes, 1, "{at:?}: pushed once");
        let rr = restore(&r.dir, &ropts(&fx)).unwrap();
        assert!(
            rr.failed.is_empty() && rr.conflicts.is_empty(),
            "{at:?}: {rr:?}"
        );
        assert_eq!(tree_digest(&r.dir), before, "{at:?}");
    }
}

#[test]
fn push_that_leaves_no_ref_keeps_the_repo() {
    // The push reports success but writes nothing: the confirming ls-remote must catch it.
    let fx = Fx::new();
    let r = repo(&fx);
    let (scan, plan) = docs(
        &[&fx.data],
        &[P::git(&r.dir, &[".env"], push_block(&r.url))],
    );
    let mut o = gopts(&fx, true, false);
    o.git = git_wrapper_with(&fx, false, "case \" $* \" in *\" push \"*) exit 0;; esac");
    o.private_remotes = vec![r.url.clone()];
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    let (out, why) = outcome(&rep, 0);
    assert_eq!(out, "kept", "{why}");
    assert!(why.contains("is not on the remote"), "{why}");
    assert!(r.dir.join(".git/HEAD").exists() && r.dir.join("new.txt").exists());
}

#[test]
fn detached_head_with_unpushed_commits_is_recovered() {
    let fx = Fx::new();
    let base = fx.tmp.path().canonicalize().unwrap();
    let bare = base.join("bare.git");
    fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "-b", "main"]);
    let dir = fx.data.join("det");
    fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    fx.write("det/a.txt", b"a\n");
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "A"]);
    let url = format!("file://{}", bare.display());
    git(&dir, &["remote", "add", "origin", &url]);
    git(&dir, &["push", "-q", "-u", "origin", "main"]);
    git(&dir, &["checkout", "-q", "--detach"]);
    fx.write("det/b.txt", b"on no branch\n");
    git(&dir, &["add", "b.txt"]);
    git(&dir, &["commit", "-q", "-m", "C (detached)"]);
    let head = git(&dir, &["rev-parse", "HEAD"]);
    let (scan, plan) = docs(&[&fx.data], &[P::git(&dir, &[], push_block(&url))]);
    let mut o = gopts(&fx, true, false);
    o.private_remotes = vec![url.clone()];
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    let (out, why) = outcome(&rep, 0);
    assert_eq!(out, "removed", "{why}");
    let date = disk_cleaner_core::apply::utc_date(o.now);
    assert!(
        recovery_refs(&bare).contains(&(format!("refs/recovery/testhost/{date}/wip/HEAD"), head)),
        "{:?}",
        recovery_refs(&bare)
    );
}

#[test]
fn replace_refs_and_symlinks_in_git_dir_never_break_restore() {
    // A replace ref would make git return other bytes for a HEAD blob.
    let fx = Fx::new();
    let r = repo(&fx);
    let blob = git(&r.dir, &["rev-parse", "HEAD:src/lib.rs"]);
    fs::write(fx.tmp.path().join("other"), b"REPLACED BYTES\n").unwrap();
    let other = git(
        &r.dir,
        &[
            "hash-object",
            "-w",
            &fx.tmp.path().join("other").display().to_string(),
        ],
    );
    git(&r.dir, &["replace", &blob, &other]);
    let (scan, plan) = docs(&[&fx.data], &[P::git(&r.dir, &[".env"], bundle_block())]);
    let mut o = gopts(&fx, true, true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    assert_eq!(outcome(&rep, 0).0, "removed", "{rep:#?}");
    let target = r.dir.join("src/lib.rs");
    let rr = restore(&target, &ropts(&fx)).unwrap();
    assert!(rr.failed.is_empty(), "{rr:?}");
    assert_eq!(fs::read(&target).unwrap(), UNCHANGED);

    // A symlink inside .git: nothing restores from the git directory; files are uploaded raw.
    let fx = Fx::new();
    let r = repo(&fx);
    let outside = fx.tmp.path().join("desc");
    fs::write(&outside, b"desc\n").unwrap();
    fs::remove_file(r.dir.join(".git/description")).unwrap();
    std::os::unix::fs::symlink(&outside, r.dir.join(".git/description")).unwrap();
    let (scan, plan) = docs(&[&fx.data], &[P::git(&r.dir, &[".env"], bundle_block())]);
    let mut o = gopts(&fx, true, true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    assert_eq!(outcome(&rep, 0).0, "removed", "{rep:#?}");
    let manifest = fs::read_to_string(fx.manifest()).unwrap();
    assert!(
        !manifest.contains("\"git_oid\":\""),
        "no file restores from the git dir"
    );
    let shards = fx.all_shard_bytes();
    assert!(shards.windows(UNCHANGED.len()).any(|w| w == UNCHANGED));
}

#[test]
fn a_plain_user_name_in_an_https_url_is_not_a_credential() {
    let fx = Fx::new();
    let r = repo(&fx);
    git(
        &r.dir,
        &[
            "remote",
            "set-url",
            "origin",
            "https://alice@bitbucket.org/team/r.git",
        ],
    );
    let (scan, plan) = docs(&[&fx.data], &[P::git(&r.dir, &[".env"], bundle_block())]);
    let mut o = gopts(&fx, true, true);
    o.oracle = Box::new(agreeing_oracle(&scan, &plan));
    let rep = apply(&scan, &plan, &o).unwrap();
    let (out, why) = outcome(&rep, 0);
    assert_eq!(out, "removed", "{why}");
}
