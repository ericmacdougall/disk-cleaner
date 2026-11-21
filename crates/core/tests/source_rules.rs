//! Rules about the library's source that its behaviour tests rely on (GoodQA goodqa#16529).
//!
//! The rescue tests observe what the scan reads through a recorder at the one place that opens
//! file content (`regular::open_regular`) and what git runs through a wrapper program given to
//! `Git::new`. That only proves something if no other code opens content or starts a process, so
//! this test reads the library's source and fails on any such call outside a short allow-list.
//! It also holds the rescue tests to planning from the scan file read back (`idle_plan`).

use std::fs;
use std::path::{Path, PathBuf};

/// Calls that open file content, copy it, or start a process.
const FORBIDDEN: &[&str] = &[
    "File::open(",
    "File::options(",
    "fs::read(",
    "fs::read_to_string(",
    "fs::copy(",
    "OpenOptions::new(",
    "Command::new(",
];

/// (file, call, why it is allowed). Anything else in crates/core/src fails the test.
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "regular.rs",
        "OpenOptions::new(",
        "the one content gate, recorded for tests",
    ),
    (
        "git.rs",
        "Command::new(",
        "the one git gate (and kill/taskkill of its own child on timeout)",
    ),
    (
        "config.rs",
        "fs::read_to_string(",
        "the tool's own config file, which the user names",
    ),
    (
        "lib.rs",
        "fs::read_to_string(",
        "read_scan: the tool's own scan file",
    ),
    ("test_fifo.rs", "Command::new(", "unit test only (mkfifo)"),
    (
        "test_fifo.rs",
        "OpenOptions::new(",
        "unit test only (opens its own FIFO)",
    ),
];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(sources(&p));
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
    out.sort();
    out
}

/// The code part of a line: what comes before a `//` comment (good enough for this source,
/// which never puts `//` inside a string next to one of the calls above).
fn code(line: &str) -> &str {
    match line.find("//") {
        Some(i) => &line[..i],
        None => line,
    }
}

#[test]
fn content_is_opened_and_processes_started_only_at_the_gates() {
    let mut bad = Vec::new();
    let mut seen = 0usize;
    for path in sources(&src_dir()) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = fs::read_to_string(&path).unwrap();
        for (n, line) in text.lines().enumerate() {
            let c = code(line);
            for call in FORBIDDEN {
                if c.contains(call) {
                    seen += 1;
                    if !ALLOWED.iter().any(|(f, a, _)| *f == name && a == call) {
                        bad.push(format!("{name}:{}: {}", n + 1, line.trim()));
                    }
                }
            }
        }
    }
    assert!(
        seen > 0,
        "the scan found no gate at all: the patterns are stale"
    );
    assert!(
        bad.is_empty(),
        "content opened or a process started outside the gates: {bad:#?}"
    );
}

#[test]
fn the_rescue_tests_plan_only_from_the_scan_file() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("rescue.rs");
    let text = fs::read_to_string(path).unwrap();
    let mut calls = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let c = code(line);
        let mut from = 0;
        while let Some(i) = c[from..].find("plan(") {
            let at = from + i;
            let before = c[..at].chars().last();
            if !before.is_some_and(|ch| ch.is_alphanumeric() || ch == '_') {
                calls.push(format!("{}: {}", n + 1, line.trim()));
            }
            from = at + 5;
        }
    }
    // Exactly the two calls inside `idle_plan`: from the scan file, and in memory to compare.
    assert_eq!(
        calls.len(),
        2,
        "plan( outside idle_plan in rescue.rs: {calls:#?}"
    );
    assert!(text.contains("let from_file = plan(&back, &opts);"));
    assert!(text.contains("let in_memory = plan(res, &opts);"));
}
