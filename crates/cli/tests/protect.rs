//! End-to-end tests of protection and the plan summary through the built binary, on a disposable
//! fixture home (goodqa#16670).

use std::fs;
use std::path::Path;
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
        .args(["-c", "core.autocrlf=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "maintenance.auto")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "gc.auto")
        .env("GIT_CONFIG_VALUE_1", "0")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn set_old(path: &Path, days: u64) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn old_project(dir: &Path, nm_bytes: usize) {
    write(&dir.join("package.json"), "{}");
    write(
        &dir.join("node_modules").join("a.js"),
        &"x".repeat(nm_bytes),
    );
    set_old(&dir.join("package.json"), 400);
    set_old(&dir.join("node_modules").join("a.js"), 400);
}

/// Point HOME at the fixture and keep the user's real config out of the run.
fn isolate(cmd: &mut Command, home: &Path) {
    cmd.env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("APPDATA", home.join("appdata"))
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME");
}

/// A fixture home with an agent home, a Dropbox folder, a pip cache, projects, a git repo that
/// needs a person and many small projects (for the top-20 cap).
fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap().join("user");
    old_project(&home.join(".claude/projects/p"), 20_000);
    write(
        &home.join(".claude/projects/p/rollout-1.jsonl"),
        &"y".repeat(50_000),
    );
    old_project(&home.join("Dropbox/proj"), 6_000);
    old_project(&home.join("work/proj"), 10_000);
    old_project(&home.join("Applications/Foo.app/Contents/Resources"), 7_000);
    write(&home.join(".cache/pip/wheel.txt"), &"w".repeat(8_000));
    set_old(&home.join(".cache/pip/wheel.txt"), 400);
    for i in 0..25 {
        old_project(&home.join(format!("work/many/p{i}")), 1_000 + i);
    }
    let repo = home.join("work/repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    write(&repo.join("README.md"), "x\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let scan_file = tmp.path().join("scan.json");
    let mut c = bin();
    c.args([
        "scan",
        home.to_str().unwrap(),
        "--no-git",
        "--home",
        home.to_str().unwrap(),
        "--out",
        scan_file.to_str().unwrap(),
    ]);
    isolate(&mut c, &home);
    let o = run(&mut c);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    // The scan summary lists the protected roots too.
    assert!(
        stdout(&o).contains("Protected roots"),
        "scan summary: {}",
        stdout(&o)
    );
    (tmp, home, scan_file)
}

fn plan_cmd(scan_file: &Path, home: &Path, json: bool) -> Output {
    let mut c = bin();
    c.args([
        "plan",
        "--scan",
        scan_file.to_str().unwrap(),
        "--trust-partial-process-check",
    ]);
    if json {
        c.arg("--json");
    }
    isolate(&mut c, home);
    run(&mut c)
}

#[test]
fn plan_summary_classes() {
    let (_tmp, home, scan_file) = fixture();

    // JSON plan.
    let o = plan_cmd(&scan_file, &home, true);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    let totals = &v["totals"];
    // By class: nested items counted once, so the classes add up to the plan's total.
    let by_class = totals["by_class"].as_object().unwrap();
    let sum: u64 = by_class
        .values()
        .map(|t| t["reclaimable_bytes"].as_u64().unwrap())
        .sum();
    assert_eq!(sum, totals["reclaimable_bytes"].as_u64().unwrap());
    // 26 removable node_modules (work/proj and the 25 small ones) and the pip cache.
    assert_eq!(by_class["regenerable"]["items"], 26, "{by_class:?}");
    assert_eq!(by_class["cache"]["items"], 1, "{by_class:?}");
    // Held: the agent home (with its bytes) and the Dropbox folder; the pip cache is not held.
    let held = totals["held"].as_array().unwrap();
    let claude = held
        .iter()
        .find(|h| h["root"] == home.join(".claude").to_str().unwrap())
        .expect("the agent home is listed");
    assert!(claude["bytes"].as_u64().unwrap() >= 70_000, "{claude}");
    // The project directory and its node_modules.
    assert_eq!(claude["items"], 2, "{claude}");
    let dropbox = held
        .iter()
        .find(|h| h["root"] == home.join("Dropbox").to_str().unwrap())
        .expect("the sync folder is listed");
    assert_eq!(dropbox["items"], 2, "{dropbox}");
    // A project inside an application package is held `package`.
    let package = held
        .iter()
        .find(|h| h["code"] == "package")
        .expect("the package directory is listed");
    assert!(package["root"]
        .as_str()
        .unwrap()
        .ends_with("/Applications/Foo.app"));
    assert_eq!(package["items"], 2, "{package}");
    let items = v["items"].as_array().unwrap();
    // NEG-8: ~/.cache/pip keeps its normal action and is not counted as held.
    let pip = items
        .iter()
        .find(|i| i["path"].as_str().unwrap().ends_with("/.cache/pip"))
        .expect("pip cache item");
    assert_eq!(pip["action"], "remove-regenerable", "{pip}");
    assert!(pip.get("hold").is_none(), "{pip}");
    let held_items = items.iter().filter(|i| i.get("hold").is_some()).count();
    let held_total: u64 = held.iter().map(|h| h["items"].as_u64().unwrap()).sum();
    assert_eq!(held_items as u64, held_total);
    // Nothing removing lies in the agent home or the sync folder.
    for it in items {
        let action = it["action"].as_str().unwrap();
        if action.contains("remove") {
            let p = it["path"].as_str().unwrap();
            assert!(!p.contains("/.claude/") && !p.contains("/Dropbox/"), "{p}");
        }
    }
    // Ask items grouped by reason code with bytes (the repo's facts are unknown under --no-git).
    let asks = totals["ask_by_reason"].as_object().unwrap();
    assert!(!asks.is_empty(), "{totals}");
    assert!(asks.values().all(|a| a["items"].as_u64().unwrap() >= 1));

    // Text plan: each section and number.
    let o = plan_cmd(&scan_file, &home, false);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let text = stdout(&o);
    for heading in [
        "By action:",
        "Reclaimable by class",
        "Protected and held back",
        "Needs a person (ask), by reason:",
        "Biggest items (top 20):",
    ] {
        assert!(text.contains(heading), "missing {heading:?} in\n{text}");
    }
    let section = |from: &str| -> Vec<String> {
        let mut it = text.lines().skip_while(|l| !l.starts_with(from));
        it.next();
        it.take_while(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    };
    let classes = section("Reclaimable by class");
    assert!(classes
        .iter()
        .any(|l| l.contains("regenerable") && l.contains("26 items")));
    assert!(classes
        .iter()
        .any(|l| l.contains("cache") && l.contains("1 item")));
    let held_lines = section("Protected and held back");
    let claude_root = home.join(".claude");
    let claude_line = held_lines
        .iter()
        .find(|l| l.ends_with(claude_root.to_str().unwrap()))
        .expect("protected root line");
    assert!(
        claude_line.contains("protected") && claude_line.contains("2 held"),
        "{claude_line}"
    );
    assert!(held_lines
        .iter()
        .any(|l| l.contains("synced") && l.contains("/Dropbox")));
    assert!(
        held_lines
            .iter()
            .any(|l| l.contains("package") && l.contains("/Applications/Foo.app")),
        "{held_lines:?}"
    );
    assert!(
        !held_lines.iter().any(|l| l.contains(".cache/pip")),
        "{held_lines:?}"
    );
    let asks_text = section("Needs a person");
    assert!(
        asks_text.iter().any(|l| l.contains("item")),
        "{asks_text:?}"
    );
    let top = section("Biggest items");
    let entries = top.iter().filter(|l| !l.starts_with("      ")).count();
    assert_eq!(entries, 20, "top 20, not more: {top:?}");
}

#[test]
fn scan_refuses_a_home_that_is_not_the_passwd_home_and_a_bad_override() {
    let tmp = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(tmp.path()).unwrap().join("user");
    fs::create_dir_all(home.join("proj")).unwrap();
    // HOME changed without --home: refused, and the message says why.
    let mut c = bin();
    c.args(["scan", home.to_str().unwrap(), "--no-git"]);
    isolate(&mut c, &home);
    let o = run(&mut c);
    assert!(!o.status.success());
    let err = format!("{:#?}", String::from_utf8_lossy(&o.stderr));
    assert!(err.contains("HOME"), "{err}");
    // A bad override with --home: refused naming the variable.
    let mut c = bin();
    c.args([
        "scan",
        home.to_str().unwrap(),
        "--no-git",
        "--home",
        home.to_str().unwrap(),
    ]);
    isolate(&mut c, &home);
    c.env("CODEX_HOME", "relative/dir");
    let o = run(&mut c);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("CODEX_HOME"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    // The same scan with a good fixture home works.
    let mut c = bin();
    c.args([
        "scan",
        home.to_str().unwrap(),
        "--no-git",
        "--home",
        home.to_str().unwrap(),
    ]);
    isolate(&mut c, &home);
    assert!(run(&mut c).status.success());
}

#[test]
fn plan_refuses_a_scan_without_protected_roots() {
    let (_tmp, home, scan_file) = fixture();
    let mut v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&scan_file).unwrap()).unwrap();
    v["protect"]["roots"] = serde_json::json!([]);
    let bad = scan_file.with_file_name("no-roots.json");
    fs::write(&bad, v.to_string()).unwrap();
    let o = plan_cmd(&bad, &home, true);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("protected_roots"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    // A uid that is not this process's.
    let mut v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&scan_file).unwrap()).unwrap();
    let uid = v["protect"]["uid"].as_u64().unwrap_or(0);
    v["protect"]["uid"] = serde_json::json!(uid + 1);
    let bad = scan_file.with_file_name("other-uid.json");
    fs::write(&bad, v.to_string()).unwrap();
    let o = plan_cmd(&bad, &home, true);
    assert!(!o.status.success());
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("uid"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}
