//! The preserve obligation: the files of a git directory that a removal must back up first.
//!
//! Scaffold only: the enumeration lands with the plan contract that uses it.

use crate::model::Node;

/// Every file in `node`'s git directory that is not an object file of its object store, as
/// paths relative to the git directory with `/` separators.
pub fn git_admin_files(node: &Node) -> std::io::Result<Vec<String>> {
    let _ = node;
    Ok(Vec::new())
}
