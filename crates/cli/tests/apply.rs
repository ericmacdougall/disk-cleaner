//! `disk-cleaner apply` through the built binary, on disposable fixture homes. This build ships
//! with the guard state unverified, so new work refuses; these tests pin that, the refusals that
//! change nothing, and `--resume` (which needs no guard).

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn bin(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_disk-cleaner"));
    c.env("HOME", home)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CODEX_SANDBOX")
        .env_remove("AGENT_HARNESS_SESSION")
        .stdin(Stdio::null());
    c
}

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    work: PathBuf,
}

fn fixture() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let work = root.join("work");
    let nm = work.join("proj/node_modules/pkg");
    fs::create_dir_all(&nm).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::write(work.join("proj/package.json"), "{}").unwrap();
    fs::write(nm.join("package.json"), "{}").unwrap();
    fs::write(nm.join("index.js"), "x".repeat(8192)).unwrap();
    fs::write(work.join(".prunedisk-sandbox"), "").unwrap();
    Fx {
        _tmp: tmp,
        root,
        home,
        work,
    }
}

fn ok(o: &Output) -> &Output {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    o
}

/// Scan and plan the work directory; returns the plan file and its id.
fn make_plan(fx: &Fx) -> (PathBuf, String) {
    let scan = fx.root.join("scan.json");
    ok(&bin(&fx.home)
        .args([
            "scan",
            fx.work.to_str().unwrap(),
            "--home",
            fx.home.to_str().unwrap(),
        ])
        .args(["--out", scan.to_str().unwrap()])
        .output()
        .unwrap());
    let far = (4_000_000_000i64).to_string();
    let o = bin(&fx.home)
        .args([
            "plan",
            "--scan",
            scan.to_str().unwrap(),
            "--json",
            "--now",
            &far,
        ])
        .output()
        .unwrap();
    ok(&o);
    let plan = fx.root.join("plan.json");
    fs::write(&plan, &o.stdout).unwrap();
    // The id is the first 16 hex characters of the file's SHA-256: ask the tool for it.
    let o = bin(&fx.home)
        .args([
            "apply",
            "--plan",
            plan.to_str().unwrap(),
            "--home",
            fx.home.to_str().unwrap(),
        ])
        .args(["--sandbox", fx.work.to_str().unwrap()])
        .output()
        .unwrap();
    ok(&o);
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let id = text
        .lines()
        .find_map(|l| l.strip_prefix("plan id: "))
        .expect("the dry run prints the plan id")
        .to_string();
    (plan, id)
}

/// Every path under `root` with its size, for "nothing changed".
fn listing(root: &Path) -> Vec<String> {
    fn walk(p: &Path, out: &mut Vec<String>) {
        let md = fs::symlink_metadata(p).unwrap();
        out.push(format!("{} {} {}", p.display(), md.len(), md.mtime()));
        if md.is_dir() {
            let mut kids: Vec<_> = fs::read_dir(p)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .collect();
            kids.sort();
            for k in kids {
                walk(&k, out);
            }
        }
    }
    let mut v = Vec::new();
    walk(root, &mut v);
    v
}

#[test]
fn apply_refusals_change_nothing() {
    let fx = fixture();
    let (plan, id) = make_plan(&fx);
    let (p, h, w) = (
        plan.to_str().unwrap(),
        fx.home.to_str().unwrap(),
        fx.work.to_str().unwrap(),
    );
    // Yesterday's unfinished journal of another plan: untouched, and listed.
    let other = "0123456789abcdef";
    let jdir = fx.home.join(".prunedisk/journal");
    fs::create_dir_all(&jdir).unwrap();
    let pd = fs::metadata(fx.work.join("proj")).unwrap();
    fs::write(
        jdir.join(format!("{other}.jsonl")),
        format!(
            "{{\"seq\":0,\"t\":0,\"rec\":\"quarantining\",\"idx\":0,\"from\":\"/x/y\",\"to\":\"/x/.prunedisk-q-z-0\",\"parent_dev\":{},\"parent_ino\":{},\"rule_id\":null}}\n",
            pd.dev(),
            pd.ino()
        ),
    )
    .unwrap();
    let state = || {
        (
            listing(&fx.work),
            fs::read(jdir.join(format!("{other}.jsonl"))).unwrap(),
        )
    };
    let before = state();

    let run = |extra: &[&str], env: Option<(&str, &str)>| {
        let mut c = bin(&fx.home);
        c.args(["apply", "--plan", p, "--home", h]).args(extra);
        if let Some((k, v)) = env {
            c.env(k, v);
        }
        c.output().unwrap()
    };
    // No plan id and a wrong one: exit 0, the id and the pending journal are printed.
    for extra in [
        vec!["--sandbox", w],
        vec![
            "--plan-id",
            "ffffffffffffffff",
            "--only",
            "regenerable",
            "--sandbox",
            w,
        ],
    ] {
        let o = run(&extra, None);
        assert_eq!(o.status.code(), Some(0));
        let t = String::from_utf8_lossy(&o.stdout);
        assert!(t.contains(&id) && t.contains(other), "{t}");
        assert_eq!(state(), before);
    }
    // Refusals (exit 2): no terminal, an agent marker, no --only, a bad sandbox, and the
    // unverified guard even inside a valid sandbox.
    #[allow(clippy::type_complexity)]
    let cases: Vec<(&str, Vec<&str>, Option<(&str, &str)>)> = vec![
        (
            "no terminal",
            vec!["--plan-id", &id, "--only", "regenerable"],
            None,
        ),
        (
            "agent marker",
            vec!["--plan-id", &id, "--only", "regenerable"],
            Some(("CLAUDECODE", "1")),
        ),
        ("no --only", vec!["--plan-id", &id, "--sandbox", w], None),
        (
            "bad sandbox",
            vec![
                "--plan-id",
                &id,
                "--only",
                "regenerable",
                "--sandbox",
                "/usr",
            ],
            None,
        ),
        (
            "unverified guard",
            vec!["--plan-id", &id, "--only", "regenerable", "--sandbox", w],
            None,
        ),
    ];
    for (what, extra, env) in cases {
        let o = run(&extra, env);
        assert_eq!(
            o.status.code(),
            Some(2),
            "{what}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert_eq!(state(), before, "{what}: nothing changed");
        assert!(
            !jdir.join(format!("{id}.jsonl")).exists(),
            "{what}: no journal"
        );
    }
    let o = run(
        &["--plan-id", &id, "--only", "regenerable", "--sandbox", w],
        None,
    );
    assert!(String::from_utf8_lossy(&o.stderr).contains("guard state"));
    assert!(fx.work.join("proj/node_modules/pkg/index.js").exists());
}

#[test]
fn apply_resume_restores_through_the_binary() {
    let fx = fixture();
    let proj = fx.work.join("proj");
    let from = proj.join("node_modules");
    let to = proj.join(".prunedisk-q-0123456789abcdef-1");
    let pd = fs::metadata(&proj).unwrap();
    let id = "0123456789abcdef";
    // The state of a run killed right after the rename: `quarantining`, the directory moved.
    fs::rename(&from, &to).unwrap();
    let jdir = fx.home.join(".prunedisk/journal");
    fs::create_dir_all(&jdir).unwrap();
    fs::write(
        jdir.join(format!("{id}.jsonl")),
        format!(
            "{{\"seq\":0,\"t\":0,\"rec\":\"quarantining\",\"idx\":1,\"from\":{},\"to\":{},\"parent_dev\":{},\"parent_ino\":{},\"rule_id\":\"node_modules\"}}\n",
            serde_json::to_string(from.to_str().unwrap()).unwrap(),
            serde_json::to_string(to.to_str().unwrap()).unwrap(),
            pd.dev(),
            pd.ino()
        ),
    )
    .unwrap();
    // No plan file, no guard state, no fresh plan: --resume still finishes the item.
    let o = bin(&fx.home)
        .args([
            "apply",
            "--resume",
            "--journal",
            id,
            "--home",
            fx.home.to_str().unwrap(),
        ])
        .args(["--sandbox", fx.work.to_str().unwrap()])
        .output()
        .unwrap();
    ok(&o);
    assert!(from.join("pkg/index.js").exists(), "restored in place");
    assert!(!to.exists());
    let text = fs::read_to_string(jdir.join(format!("{id}.jsonl"))).unwrap();
    assert!(text.contains("restored-in-place"), "{text}");
    // A bad journal name is refused before it can become a path.
    let o = bin(&fx.home)
        .args([
            "apply",
            "--resume",
            "--journal",
            "../../etc/x",
            "--home",
            fx.home.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
}
