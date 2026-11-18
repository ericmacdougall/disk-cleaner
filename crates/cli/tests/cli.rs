//! End-to-end tests of the `disk-cleaner` binary on synthetic fixtures.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_disk-cleaner"))
}

fn run(cmd: &mut Command) -> Output {
    cmd.output().expect("run disk-cleaner")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Names and sizes of everything under `dir`, sorted.
fn listing(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let md = e.metadata().unwrap();
            out.push((e.path(), md.len()));
            if md.is_dir() {
                stack.push(e.path());
            }
        }
    }
    out.sort();
    out
}

fn far_future() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    (now + 3 * 365 * 86_400).to_string()
}

#[test]
fn scan_then_plan_end_to_end_without_leaking_the_remote_token() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("work dir");
    let repo = root.join("app");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    write(&repo.join("package.json"), "{}");
    write(&repo.join("node_modules").join("dep").join("i.js"), "x");
    git(&repo, &["add", "package.json"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(
        &repo,
        &["remote", "add", "origin", "https://carol:cli-SECRET-tok-77@example.invalid/x.git"],
    );
    let out_file = tmp.path().join("scan.json");

    let o = run(bin().args(["scan", root.to_str().unwrap(), "--out", out_file.to_str().unwrap()]));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let human = stdout(&o);
    assert!(human.contains("Repos and worktrees"), "{human}");
    assert!(human.contains("Space by class"));

    let json = fs::read_to_string(&out_file).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["schema_version"], 1);

    let o = run(bin().args(["plan", "--scan", out_file.to_str().unwrap(), "--idle-days", "30"])
        .args(["--now", &far_future()]));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let plan_text = stdout(&o);
    assert!(plan_text.contains("Dry-run plan"));

    let o = run(bin().args(["plan", "--scan", out_file.to_str().unwrap(), "--idle-days", "30", "--json"])
        .args(["--now", &far_future()]));
    let plan_json = stdout(&o);
    let pv: serde_json::Value = serde_json::from_str(&plan_json).unwrap();
    assert_eq!(pv["dry_run"], true);
    // The only remote has no refs containing HEAD, so the repo is never offered for archive.
    let repo_path = repo.to_string_lossy().into_owned();
    let repo_item = pv["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["path"].as_str() == Some(repo_path.as_str()))
        .expect("repo item");
    assert_eq!(repo_item["action"], "recover-then-archive");

    for text in [&human, &json, &plan_text, &plan_json] {
        assert!(!text.contains("cli-SECRET-tok-77"), "token leaked");
        assert!(!text.contains("carol"), "user leaked");
    }
    // The URL itself is kept, credential-free.
    assert!(json.contains("https://example.invalid/x.git"));
}

#[test]
fn scan_json_on_stdout_and_unicode_paths_round_trip_into_the_plan() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("scan");
    let proj = root.join("café ☕").join("проект one");
    write(&proj.join("package.json"), "{}");
    write(&proj.join("node_modules").join("a.js"), "1");
    let before = listing(&root);

    let o = run(bin().args(["scan", root.to_str().unwrap(), "--json", "--no-git", "--one-file-system"]));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let scan_json = stdout(&o);
    let v: serde_json::Value = serde_json::from_str(&scan_json).unwrap();
    let paths: Vec<String> = v["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["path"].as_str().unwrap().to_string())
        .collect();
    assert!(paths.contains(&proj.to_string_lossy().into_owned()), "{paths:?}");
    // Read-only: scanning changed nothing.
    assert_eq!(before, listing(&root));

    let scan_file = tmp.path().join("scan.json");
    fs::write(&scan_file, &scan_json).unwrap();
    let o = run(bin().args(["plan", "--scan", scan_file.to_str().unwrap(), "--idle-days", "1", "--json"])
        .args(["--now", &far_future()]));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let pv: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let items: Vec<&str> = pv["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["path"].as_str().unwrap())
        .collect();
    let proj_path = proj.to_string_lossy().into_owned();
    assert!(items.contains(&proj_path.as_str()));
    // Loose code is always ask.
    let loose = pv["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["path"].as_str() == Some(proj_path.as_str()))
        .unwrap();
    assert_eq!(loose["action"], "ask");
}

#[test]
fn active_flag_keeps_everything_under_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("scan");
    let proj = root.join("proj");
    write(&proj.join("package.json"), "{}");
    write(&proj.join("node_modules").join("a.js"), "1");
    let scan_file = tmp.path().join("scan.json");
    let o = run(bin().args(["scan", root.to_str().unwrap(), "--no-git", "--out", scan_file.to_str().unwrap()]));
    assert!(o.status.success());

    let plan = |active: Option<&Path>| -> serde_json::Value {
        let mut c = bin();
        c.args(["plan", "--scan", scan_file.to_str().unwrap(), "--idle-days", "1", "--json"])
            .args(["--now", &far_future()]);
        if let Some(a) = active {
            c.args(["--active", a.to_str().unwrap()]);
        }
        let o = run(&mut c);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        serde_json::from_str(&stdout(&o)).unwrap()
    };
    let nm = proj.join("node_modules").to_string_lossy().into_owned();
    let action = |v: &serde_json::Value| -> String {
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"].as_str() == Some(nm.as_str()))
            .unwrap()["action"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(action(&plan(None)), "remove-regenerable");
    assert_eq!(action(&plan(Some(&proj))), "keep");
}

#[test]
fn plan_requires_an_explicit_idle_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("scan.json");
    fs::write(&f, "{}").unwrap();
    let o = run(bin().args(["plan", "--scan", f.to_str().unwrap()]));
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("idle-days"));
}

#[test]
fn bad_scan_file_and_missing_root_fail_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("scan.json");
    fs::write(&f, "{\"schema_version\": 99}").unwrap();
    let o = run(bin().args(["plan", "--scan", f.to_str().unwrap(), "--idle-days", "1"]));
    assert!(!o.status.success());
    let o = run(bin().args(["scan", tmp.path().join("nope").to_str().unwrap()]));
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("cannot"));
}
