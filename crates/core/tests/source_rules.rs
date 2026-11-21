//! Rules about the library's source that its behaviour tests rely on (GoodQA goodqa#16529).
//!
//! The rescue tests observe what the scan reads through a recorder at the one place that opens
//! file content (`regular::open_regular`) and what git runs through a wrapper program given to
//! `Git::new`. That only proves something if no other code on the scan and plan path opens content
//! or starts a process, so this test reads the library's source and fails on any such call outside
//! a short allow-list. It leaves one directory, `src/apply/`, to that module's own rule (see
//! `EXCLUDED_DIR`). It also holds the rescue tests to planning from the scan file read back
//! (`idle_plan`).

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

/// The one directory below crates/core/src this rule leaves to another rule, and why. It is a
/// directory prefix of the path relative to src/ ("apply/..."), never a file name and never a
/// directory of that name further down.
const EXCLUDED_DIR: (&str, &str) = (
    "apply/",
    "apply is the module that writes state and runs storage and git tools; its gates are \
     guarded by apply's own source rule (tests/apply_source_rules.rs, owned by the apply stream)",
);

/// (path relative to crates/core/src with `/` separators, call, why it is allowed). Anything else
/// outside `EXCLUDED_DIR` fails the test; a file elsewhere with the same name is not allowed.
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

/// A source file's path relative to src/, with `/` separators on every OS.
fn relative(src: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(src).unwrap();
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.join("/")
}

/// The rule over (path relative to src/, source text) pairs: how many forbidden calls it saw
/// outside `EXCLUDED_DIR`, and each one the allow-list does not name.
fn findings(files: &[(String, String)]) -> (usize, Vec<String>) {
    let mut bad = Vec::new();
    let mut seen = 0usize;
    for (rel, text) in files {
        if rel.starts_with(EXCLUDED_DIR.0) {
            continue;
        }
        for (n, line) in text.lines().enumerate() {
            let c = code(line);
            for call in FORBIDDEN {
                if c.contains(call) {
                    seen += 1;
                    if !ALLOWED.iter().any(|(f, a, _)| f == rel && a == call) {
                        bad.push(format!("{rel}:{}: {}", n + 1, line.trim()));
                    }
                }
            }
        }
    }
    (seen, bad)
}

#[test]
fn content_is_opened_and_processes_started_only_at_the_gates() {
    let src = src_dir();
    let files: Vec<(String, String)> = sources(&src)
        .iter()
        .map(|p| (relative(&src, p), fs::read_to_string(p).unwrap()))
        .collect();
    let left = files
        .iter()
        .filter(|(rel, _)| rel.starts_with(EXCLUDED_DIR.0))
        .count();
    eprintln!(
        "source_rules: {} files, {left} under src/{} left to its own rule: {}",
        files.len(),
        EXCLUDED_DIR.0,
        EXCLUDED_DIR.1
    );
    let (seen, bad) = findings(&files);
    assert!(
        seen > 0,
        "the scan found no gate at all: the patterns are stale"
    );
    assert!(
        bad.is_empty(),
        "content opened or a process started outside the gates: {bad:#?}"
    );
}

/// The exclusion is the src/apply/ directory and nothing else: a forbidden call planted anywhere
/// outside it still fails, and the allow-list goes by path and by call.
#[test]
fn a_forbidden_call_outside_src_apply_still_fails() {
    let one = |rel: &str, line: &str| findings(&[(rel.to_string(), line.to_string())]);
    let open = "    let f = std::fs::File::open(path)?;";
    // Below src/apply/, at any depth: apply's own rule covers it.
    assert_eq!(one("apply/run.rs", open), (0, vec![]));
    assert_eq!(one("apply/storage/inner.rs", open), (0, vec![]));
    // Everywhere else it fails: beside apply, a file or directory whose name starts with
    // "apply", an apply/ directory further down, and the scan and plan modules themselves.
    for rel in [
        "pieces.rs",
        "scan.rs",
        "apply.rs",
        "applied/run.rs",
        "scan/apply/run.rs",
    ] {
        let (seen, bad) = one(rel, open);
        assert_eq!((seen, bad.len()), (1, 1), "{rel}: {bad:?}");
        assert!(bad[0].starts_with(&format!("{rel}:1:")), "{rel}: {bad:?}");
    }
    // The allow-list names a path and a call: the content gate's own call passes in regular.rs,
    // fails in a file of that name elsewhere, and another call fails in regular.rs.
    let gate = "    let mut opts = std::fs::OpenOptions::new();";
    assert_eq!(one("regular.rs", gate), (1, vec![]));
    assert_eq!(one("x/regular.rs", gate).1.len(), 1);
    assert_eq!(one("regular.rs", open).1.len(), 1);
    // A call named only in a comment is not a call.
    assert_eq!(
        one("pieces.rs", "    // File::open( is the gate's job"),
        (0, vec![])
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
