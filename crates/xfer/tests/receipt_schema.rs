//! AC-12 (shared fixture): the receipt schema v1 that a deleter consumes.
//!
//! `tests/fixtures/receipt-v1.jsonl` is the contract with the apply stream. It holds one real
//! receipt per shape the engine emits (file, empty file, dir, symlink, hard-link member and a
//! non-UTF-8 name). These tests fail when:
//! - a fixture line no longer deserializes, or re-serializes to different JSON;
//! - any required field can be dropped without the parse failing;
//! - a fresh backup emits a receipt whose shape (its set of keys) the fixture does not have.
//!
//! Regenerate deliberately with `XFER_WRITE_FIXTURE=1 cargo test --test receipt_schema`, and
//! tell the consumers: a changed shape is a schema change.

mod common;
use common::*;
use serde_json::Value;
use std::collections::BTreeSet;
use xfer::receipt::Receipt;

const FIXTURE: &str = include_str!("fixtures/receipt-v1.jsonl");

/// Fields every v1 receipt carries.
const REQUIRED: &[&str] = &[
    "v",
    "engine",
    "repo",
    "run",
    "idx",
    "idx.key",
    "idx.size",
    "idx.hash",
    "files",
    "files.key",
    "files.size",
    "files.hash",
    "kind",
    "id",
    "id.dev",
    "id.ino",
    "id.size",
    "id.mtime_ns",
    "id.ctime_ns",
    "id.nlink",
    "mode",
    "alloc_bytes",
    "meta_incomplete",
];

/// The dotted key set of a receipt line, with its kind: the receipt's shape.
fn shape(v: &Value) -> (String, BTreeSet<String>) {
    let mut keys = BTreeSet::new();
    for (k, x) in v.as_object().expect("object") {
        keys.insert(k.clone());
        if let Some(o) = x.as_object() {
            for k2 in o.keys() {
                keys.insert(format!("{k}.{k2}"));
            }
        }
    }
    (v["kind"].as_str().expect("kind").to_string(), keys)
}

fn fixture_lines() -> Vec<Value> {
    FIXTURE
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("fixture line is JSON"))
        .collect()
}

#[test]
fn fixture_parses_round_trips_and_covers_every_kind() {
    let lines = fixture_lines();
    let mut kinds = BTreeSet::new();
    let mut hex = false;
    for v in &lines {
        let r: Receipt = serde_json::from_value(v.clone()).expect("fixture line is a Receipt");
        assert_eq!(r.v, 1);
        let back = serde_json::to_value(&r).expect("serialize");
        assert_eq!(
            &back, v,
            "a fixture line must re-serialize to the same JSON"
        );
        let (kind, keys) = shape(v);
        for k in REQUIRED {
            assert!(keys.contains(*k), "{kind} receipt lacks required key {k}");
        }
        assert!(
            keys.contains("path") ^ keys.contains("path_hex"),
            "exactly one of path and path_hex"
        );
        // a hard-link member's content hash is on its group's `file` receipt (same link_group)
        assert_eq!(
            keys.contains("blake3"),
            kind == "file",
            "blake3 is present exactly on file receipts ({kind})"
        );
        if kind == "hardlink" {
            assert!(
                keys.contains("link_group"),
                "a hardlink receipt names its group"
            );
        }
        hex |= keys.contains("path_hex");
        kinds.insert(kind);
    }
    for k in ["file", "dir", "symlink", "hardlink"] {
        assert!(kinds.contains(k), "the fixture has no {k} receipt");
    }
    assert!(hex, "the fixture has no non-UTF-8 (path_hex) receipt");
}

#[test]
fn dropping_any_required_field_fails_the_parse() {
    let line = fixture_lines()
        .into_iter()
        .find(|v| v["kind"] == "file")
        .expect("a file line");
    for k in REQUIRED {
        let mut v = line.clone();
        match k.split_once('.') {
            Some((a, b)) => {
                v[a].as_object_mut().expect("nested").remove(b);
            }
            None => {
                v.as_object_mut().expect("object").remove(*k);
            }
        }
        assert!(
            serde_json::from_value::<Receipt>(v).is_err(),
            "a receipt without {k} must not parse"
        );
    }
}

#[test]
fn fresh_receipts_have_only_fixture_shapes() {
    let env = Env::new();
    let src = env.p("src");
    sample_tree(&src);
    let rcp = env.p("receipts.jsonl");
    let rep = xfer::backup::backup(env.local(), &opts(&src, &rcp)).expect("backup");
    assert_eq!(rep.exit_code, 0, "{rep:#?}");
    let fresh: Vec<Value> = std::fs::read_to_string(&rcp)
        .expect("receipts")
        .lines()
        .map(|l| serde_json::from_str(l).expect("json"))
        .collect();
    assert!(!fresh.is_empty());

    if std::env::var("XFER_WRITE_FIXTURE").as_deref() == Ok("1") {
        // one receipt per shape, in a stable order
        let mut seen = BTreeSet::new();
        let mut out = String::new();
        let mut sorted = fresh.clone();
        sorted.sort_by_key(|v| {
            (
                v["kind"].as_str().unwrap_or("").to_string(),
                v["path"].as_str().unwrap_or("").to_string(),
            )
        });
        for v in sorted {
            if seen.insert(shape(&v)) {
                out.push_str(&serde_json::to_string(&v).expect("json"));
                out.push('\n');
            }
        }
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/receipt-v1.jsonl");
        std::fs::write(&p, out).expect("write fixture");
        eprintln!("wrote {}", p.display());
        return;
    }

    let known: BTreeSet<_> = fixture_lines().iter().map(shape).collect();
    for v in &fresh {
        let s = shape(v);
        assert!(
            known.contains(&s),
            "the engine emitted a receipt shape the fixture does not have: {s:?}"
        );
    }
}
