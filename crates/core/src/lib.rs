//! disk-cleaner core: scan, classify, repo safety facts and dry-run planning.
//!
//! This crate is read-only. It has no UI and no network code, and it contains no code path
//! that deletes, moves or writes files (the only write is [`write_json`], used by the CLI for
//! its own output file) or runs a mutating git command.

#![forbid(unsafe_code)]

pub mod config;
pub mod error;
pub mod facts;
pub mod git;
pub mod model;
pub mod plan;
pub mod procs;
pub mod redact;
pub mod rules;
pub mod scan;

pub use config::{Config, ConfigSource, DEFAULT_ACTIVE_WINDOW_DAYS};
pub use error::{Error, Result};
pub use facts::{is_network_url, FactsConfig};
pub use git::{Git, GitError};
pub use model::*;
pub use plan::{plan, resolve_active, Action, Plan, PlanItem, PlanOptions, ProjectState};
pub use procs::{running_cwds, ProcessCheck};
pub use rules::RuleSet;
pub use scan::{scan, ScanOptions};

use serde::Serialize;
use std::path::Path;

/// Serialize `value` as pretty JSON to `path`. This is the only file the library writes.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    std::fs::write(path, text).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Read a scan file written by [`write_json`], checking its schema version.
pub fn read_scan(path: &Path) -> Result<ScanResult> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    scan_from_json(&text)
}

/// Parse scan JSON, checking its schema version first.
pub fn scan_from_json(text: &str) -> Result<ScanResult> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    let found = value
        .get("schema_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    if found != SCHEMA_VERSION {
        return Err(Error::Schema {
            found,
            expected: SCHEMA_VERSION,
        });
    }
    let scan: ScanResult = serde_json::from_value(value)?;
    validate_scan(&scan)?;
    Ok(scan)
}

/// Sum `values` with checked arithmetic.
fn checked_total(what: &str, mut values: impl Iterator<Item = u64>) -> Result<()> {
    values
        .try_fold(0u64, |a, v| a.checked_add(v))
        .map(|_| ())
        .ok_or_else(|| invalid(what, "the sum overflows 64 bits"))
}

fn invalid(field: impl Into<String>, message: impl Into<String>) -> Error {
    Error::InvalidScan {
        field: field.into(),
        message: message.into(),
    }
}

/// Check that a scan document is internally consistent before anything plans from it: every
/// index into `nodes` is in range, the parent links form no cycle (checked iteratively, in one
/// pass, whatever the order of `nodes`), and the byte and file counts do not overflow when summed.
/// The first violation is returned with the field and index.
pub fn validate_scan(scan: &ScanResult) -> Result<()> {
    let n = scan.nodes.len();
    for (i, node) in scan.nodes.iter().enumerate() {
        if let Some(p) = node.parent {
            if p >= n {
                return Err(invalid(
                    format!("nodes[{i}].parent"),
                    format!("index {p} is out of range ({n} nodes)"),
                ));
            }
            if p == i {
                return Err(invalid(
                    format!("nodes[{i}].parent"),
                    "a node cannot be its own parent",
                ));
            }
        }
        if let Some(pn) = node.repo.as_ref().and_then(|r| r.parent_node) {
            if pn >= n {
                return Err(invalid(
                    format!("nodes[{i}].repo.parent_node"),
                    format!("index {pn} is out of range ({n} nodes)"),
                ));
            }
        }
    }
    for (j, f) in scan.big_files.iter().enumerate() {
        if let Some(k) = f.node.filter(|k| *k >= n) {
            return Err(invalid(
                format!("big_files[{j}].node"),
                format!("index {k} is out of range ({n} nodes)"),
            ));
        }
    }
    for (j, f) in scan.outputs.iter().enumerate() {
        if let Some(k) = f.node.filter(|k| *k >= n) {
            return Err(invalid(
                format!("outputs[{j}].node"),
                format!("index {k} is out of range ({n} nodes)"),
            ));
        }
    }
    // Cycles: 0 unseen, 1 on the current walk, 2 finished.
    let mut state = vec![0u8; n];
    for start in 0..n {
        if state[start] != 0 {
            continue;
        }
        let mut path: Vec<usize> = Vec::new();
        let mut cur = Some(start);
        while let Some(c) = cur {
            match state[c] {
                2 => break,
                1 => {
                    return Err(invalid(
                        format!("nodes[{c}].parent"),
                        "the parent links form a cycle",
                    ))
                }
                _ => {
                    state[c] = 1;
                    path.push(c);
                    cur = scan.nodes[c].parent;
                }
            }
        }
        for c in path {
            state[c] = 2;
        }
    }
    // Sums: checked, so a hostile file cannot wrap a total.
    checked_total(
        "nodes[].bytes_self",
        scan.nodes.iter().map(|x| x.bytes_self),
    )?;
    checked_total(
        "nodes[].bytes_total",
        scan.nodes.iter().map(|x| x.bytes_total),
    )?;
    checked_total(
        "nodes[].files_total",
        scan.nodes.iter().map(|x| x.files_total),
    )?;
    checked_total(
        "nodes[].output_bytes",
        scan.nodes.iter().map(|x| x.output_bytes),
    )?;
    checked_total("outputs[].bytes", scan.outputs.iter().map(|x| x.bytes))?;
    checked_total("big_files[].bytes", scan.big_files.iter().map(|x| x.bytes))?;
    Ok(())
}
