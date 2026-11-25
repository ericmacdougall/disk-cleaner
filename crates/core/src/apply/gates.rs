//! Class A gates that need no host state: the plan id, the courtesy check and the `--sandbox`
//! validity rule. The courtesy check is not a control (the control is the harness hook): it stops
//! a careless run, nothing more.

use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

/// The plan id: the first 16 hex characters of the SHA-256 of the plan file's bytes.
pub fn plan_id(bytes: &[u8]) -> String {
    let mut s = super::fsops::hex(&Sha256::digest(bytes));
    s.truncate(16);
    s
}

/// Environment variables that mark an agent session. `CODEX_HOME` is user configuration, not a
/// marker, and is not in this list.
pub const AGENT_MARKERS: [&str; 4] = [
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_SANDBOX",
    "AGENT_HARNESS_SESSION",
];

/// Why the courtesy check refuses, or `None` when the run looks like a person's own terminal.
pub fn courtesy_refusal(
    stdin_is_terminal: bool,
    getenv: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    if !stdin_is_terminal {
        return Some("standard input is not a terminal".to_string());
    }
    AGENT_MARKERS
        .iter()
        .find(|k| getenv(k).is_some())
        .map(|k| format!("{k} is set: this looks like an agent session"))
}

/// Temporary roots a sandbox may live under: `/tmp`, `/var/folders` and `$TMPDIR`, as spelled
/// and canonicalised.
fn temp_roots(getenv: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let mut spelled: Vec<PathBuf> = vec!["/tmp".into(), "/var/folders".into()];
    if let Some(t) = getenv("TMPDIR").filter(|t| !t.is_empty()) {
        spelled.push(PathBuf::from(t));
    }
    let mut all = Vec::new();
    for s in spelled {
        if let Ok(c) = std::fs::canonicalize(&s) {
            all.push(c);
        }
        all.push(s);
    }
    all
}

/// Is `--sandbox DIR` valid? DIR is canonicalised (so macOS `/tmp` resolves to `/private/tmp`);
/// the canonical path lies under a canonical temporary root and outside the home directory; no
/// component of the spelled path below the temporary root is a symlink; DIR holds
/// `.prunedisk-sandbox`; it contains no protected root and lies inside none; and it contains
/// every candidate. `Err` says which rule failed.
pub fn validate_sandbox(
    dir: &Path,
    home: &Path,
    protected: &[PathBuf],
    candidates: &[PathBuf],
    getenv: &dyn Fn(&str) -> Option<String>,
) -> Result<PathBuf, String> {
    let spelled = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("cannot read the current directory: {e}"))?
            .join(dir)
    };
    let canon =
        std::fs::canonicalize(&spelled).map_err(|e| format!("--sandbox {}: {e}", dir.display()))?;
    let roots = temp_roots(getenv);
    let canon_roots: Vec<&PathBuf> = roots.iter().filter(|r| canon.starts_with(r)).collect();
    if canon_roots.is_empty() {
        return Err("--sandbox must lie under /tmp, /var/folders or $TMPDIR".to_string());
    }
    let home_canon = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    if canon.starts_with(&home_canon) || canon.starts_with(home) {
        return Err("--sandbox must lie outside the home directory".to_string());
    }
    // A link below the temporary root would let the sandbox point anywhere.
    let below_root = roots
        .iter()
        .filter(|r| spelled.starts_with(r))
        .map(|r| spelled.strip_prefix(r).unwrap_or(&spelled).to_path_buf())
        .next()
        .ok_or_else(|| "--sandbox must be spelled under a temporary root".to_string())?;
    let root_spelled = roots
        .iter()
        .find(|r| spelled.starts_with(r))
        .cloned()
        .unwrap_or_default();
    let mut walk = root_spelled;
    for c in below_root.components() {
        match c {
            Component::Normal(n) => walk.push(n),
            _ => return Err("--sandbox must not contain `.` or `..`".to_string()),
        }
        let md = std::fs::symlink_metadata(&walk)
            .map_err(|e| format!("--sandbox {}: {e}", walk.display()))?;
        if md.file_type().is_symlink() {
            return Err(format!("--sandbox: {} is a symlink", walk.display()));
        }
    }
    if !canon.join(".prunedisk-sandbox").is_file() {
        return Err("--sandbox must hold a .prunedisk-sandbox file".to_string());
    }
    for r in protected {
        if r.starts_with(&canon) || canon.starts_with(r) {
            return Err(format!(
                "--sandbox overlaps the protected root {}",
                r.display()
            ));
        }
    }
    for c in candidates {
        // The parent is resolved, the item's own name is not (a link is never followed).
        let real = match (c.parent(), c.file_name()) {
            (Some(p), Some(n)) => std::fs::canonicalize(p)
                .map(|p| p.join(n))
                .unwrap_or_else(|_| c.clone()),
            _ => c.clone(),
        };
        if !real.starts_with(&canon) {
            return Err(format!(
                "--sandbox does not contain the candidate {}",
                c.display()
            ));
        }
    }
    Ok(canon)
}
