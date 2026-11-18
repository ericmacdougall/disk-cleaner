//! disk-cleaner core: scan, classify, repo safety facts and dry-run planning.
//!
//! This crate is read-only. It has no UI and no network code, and it contains no code path
//! that deletes, moves or writes files (the only write is [`write_json`], used by the CLI for
//! its own output file) or runs a mutating git command.

#![forbid(unsafe_code)]

pub mod error;
pub mod facts;
pub mod git;
pub mod model;
pub mod plan;
pub mod redact;
pub mod rules;
pub mod scan;

pub use error::{Error, Result};
pub use facts::{is_network_url, FactsConfig};
pub use git::{Git, GitError};
pub use model::*;
pub use plan::{plan, Action, Plan, PlanItem, PlanOptions};
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
    Ok(serde_json::from_value(value)?)
}
