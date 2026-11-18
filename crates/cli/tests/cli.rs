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
        &[
            "remote",
            "add",
            "origin",
            "https://carol:cli-SECRET-tok-77@example.invalid/x.git",
        ],
    );
    let out_file = tmp.path().join("scan.json");

    let o = run(bin().args([
        "scan",
        root.to_str().unwrap(),
        "--out",
        out_file.to_str().unwrap(),
    ]));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let human = stdout(&o);
    assert!(human.contains("Repos and worktrees"), "{human}");
    assert!(human.contains("Space by class"));

    let json = fs::read_to_string(&out_file).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["schema_version"], 1);

    let o = run(bin()
        .args([
            "plan",
            "--scan",
            out_file.to_str().unwrap(),
            "--active-window-days",
            "30",
        ])
        .args(["--now", &far_future()]));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let plan_text = stdout(&o);
    assert!(plan_text.contains("Dry-run plan"));

    let o = run(bin()
        .args([
            "plan",
            "--scan",
            out_file.to_str().unwrap(),
            "--active-window-days",
            "30",
            "--json",
        ])
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

    let o = run(bin().args([
        "scan",
        root.to_str().unwrap(),
        "--json",
        "--no-git",
        "--one-file-system",
    ]));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let scan_json = stdout(&o);
    let v: serde_json::Value = serde_json::from_str(&scan_json).unwrap();
    let paths: Vec<String> = v["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["path"].as_str().unwrap().to_string())
        .collect();
    assert!(
        paths.contains(&proj.to_string_lossy().into_owned()),
        "{paths:?}"
    );
    // Read-only: scanning changed nothing.
    assert_eq!(before, listing(&root));

    let scan_file = tmp.path().join("scan.json");
    fs::write(&scan_file, &scan_json).unwrap();
    let o = run(bin()
        .args([
            "plan",
            "--scan",
            scan_file.to_str().unwrap(),
            "--active-window-days",
            "1",
            "--json",
        ])
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
    let o = run(bin().args([
        "scan",
        root.to_str().unwrap(),
        "--no-git",
        "--out",
        scan_file.to_str().unwrap(),
    ]));
    assert!(o.status.success());

    let plan = |active: Option<&Path>| -> serde_json::Value {
        let mut c = bin();
        c.args([
            "plan",
            "--scan",
            scan_file.to_str().unwrap(),
            "--active-window-days",
            "1",
            "--json",
        ])
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

/// Point every platform's config-dir variable at `home` so the user's real config is never read.
fn isolate(cmd: &mut Command, home: &Path) {
    cmd.env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg"))
        .env("APPDATA", home.join("appdata"));
}

/// Where the tool looks for its config when none is named, under an isolated `home`.
fn default_config_under(home: &Path) -> PathBuf {
    let base = if cfg!(windows) {
        home.join("appdata")
    } else if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else {
        home.join("xdg")
    };
    base.join("disk-cleaner").join("config.toml")
}

/// A scan of a project whose only own file was last touched `days` ago, with a node_modules.
fn scan_with_project_touched(tmp: &Path, days: u64) -> (PathBuf, String) {
    let root = tmp.join("scan");
    let proj = root.join("proj");
    write(&proj.join("package.json"), "{}");
    write(&proj.join("node_modules").join("a.js"), "1");
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
    fs::OpenOptions::new()
        .write(true)
        .open(proj.join("package.json"))
        .unwrap()
        .set_modified(t)
        .unwrap();
    let scan_file = tmp.join("scan.json");
    let o = run(bin().args([
        "scan",
        root.to_str().unwrap(),
        "--no-git",
        "--out",
        scan_file.to_str().unwrap(),
    ]));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    (
        scan_file,
        proj.join("node_modules").to_string_lossy().into_owned(),
    )
}

fn plan_json(scan_file: &Path, home: &Path, extra: &[&str]) -> Output {
    let mut c = bin();
    c.args(["plan", "--scan", scan_file.to_str().unwrap(), "--json"])
        .args(extra);
    isolate(&mut c, home);
    run(&mut c)
}

fn nm_action(o: &Output, nm: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(&stdout(o)).expect("plan json");
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["path"].as_str() == Some(nm))
        .expect("node_modules item")["action"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn neg11_default_window_is_seven_days_and_a_shorter_one_changes_the_answer() {
    let tmp = tempfile::tempdir().unwrap();
    let (scan_file, nm) = scan_with_project_touched(tmp.path(), 6);
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let o = plan_json(&scan_file, &home, &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["active_window_days"], 7);
    assert_eq!(v["window_source"], "default");
    assert!(v["process_check"].is_string());
    assert_eq!(nm_action(&o, &nm), "keep");

    let o = plan_json(&scan_file, &home, &["--active-window-days", "3"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let a = nm_action(&o, &nm);
    assert_ne!(a, "keep", "6 days is outside a 3-day window");
    assert!(a == "remove-regenerable" || a == "ask", "{a}");

    // The human output states the window and why each project was judged.
    let mut c = bin();
    c.args(["plan", "--scan", scan_file.to_str().unwrap()]);
    isolate(&mut c, &home);
    let text = stdout(&run(&mut c));
    assert!(text.contains("Active window: 7 days"), "{text}");
    assert!(text.contains("within the 7-day active window"), "{text}");
}

#[test]
fn config_file_sets_the_window_and_the_flag_overrides_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (scan_file, nm) = scan_with_project_touched(tmp.path(), 6);
    let home = tmp.path().join("home");
    let cfg = default_config_under(&home);
    write(&cfg, "active_window_days = 3\n");

    // Found at the platform location.
    let o = plan_json(&scan_file, &home, &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_ne!(nm_action(&o, &nm), "keep");
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert_eq!(v["active_window_days"], 3);

    // The flag wins over the file.
    let o = plan_json(&scan_file, &home, &["--active-window-days", "30"]);
    assert_eq!(nm_action(&o, &nm), "keep");

    // --config names another file.
    let other = tmp.path().join("other.toml");
    write(&other, "active_window_days = 30\n");
    let o = plan_json(&scan_file, &home, &["--config", other.to_str().unwrap()]);
    assert_eq!(nm_action(&o, &nm), "keep");
}

#[test]
fn neg12_malformed_config_is_an_error_never_a_silent_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    let (scan_file, _nm) = scan_with_project_touched(tmp.path(), 6);
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();

    for bad in [
        "active_window_days = \"soon\"\n",
        "active_window_days = -2\n",
        "active_window_days = \n",
        "not toml at all\n",
        "active_window_dayz = 3\n",
    ] {
        let cfg = tmp.path().join("bad.toml");
        write(&cfg, bad);
        // Named explicitly, even when the flag would override the value.
        for extra in [
            vec!["--config", cfg.to_str().unwrap()],
            vec![
                "--config",
                cfg.to_str().unwrap(),
                "--active-window-days",
                "30",
            ],
        ] {
            let o = plan_json(&scan_file, &home, &extra);
            assert!(!o.status.success(), "accepted {bad:?}");
            assert!(stdout(&o).trim().is_empty(), "no plan on a bad config");
            let err = String::from_utf8_lossy(&o.stderr).into_owned();
            assert!(err.contains("bad.toml"), "{err}");
        }
        // And at the default location.
        write(&default_config_under(&home), bad);
        let o = plan_json(&scan_file, &home, &[]);
        assert!(
            !o.status.success(),
            "default-location config {bad:?} accepted"
        );
        assert!(String::from_utf8_lossy(&o.stderr).contains("config.toml"));
        fs::remove_file(default_config_under(&home)).unwrap();
    }

    // A named config that does not exist is also an error.
    let o = plan_json(
        &scan_file,
        &home,
        &["--config", tmp.path().join("absent.toml").to_str().unwrap()],
    );
    assert!(!o.status.success());
}

#[test]
fn bad_scan_file_and_missing_root_fail_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("scan.json");
    fs::write(&f, "{\"schema_version\": 99}").unwrap();
    let o = run(bin().args([
        "plan",
        "--scan",
        f.to_str().unwrap(),
        "--active-window-days",
        "1",
    ]));
    assert!(!o.status.success());
    let o = run(bin().args(["scan", tmp.path().join("nope").to_str().unwrap()]));
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("cannot"));
}
