//! The git lane (placeholder until the next commit): every git piece is kept.

use super::input::{Inputs, PlanPiece};
use super::recheck::Enumerated;
use super::run::Prepared;
use super::ApplyOptions;
use std::path::Path;

pub(crate) fn prepare(
    _opts: &ApplyOptions,
    _inputs: &Inputs,
    _p: &PlanPiece,
    _en: &Enumerated,
) -> Result<Prepared, String> {
    Err("the git lane is not built yet; kept".into())
}

/// Raw bytes of a blob from a git directory, without filters.
pub(crate) fn cat_blob(git: &Path, git_dir: &Path, oid: &str) -> Result<Vec<u8>, String> {
    let out = std::process::Command::new(git)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_DIR", git_dir)
        .env("GIT_NO_LAZY_FETCH", "1")
        .args(["cat-file", "blob", oid])
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!("git cat-file failed for {oid}"));
    }
    Ok(out.stdout)
}
