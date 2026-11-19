//! Read-only view model shared by the terminal view (`crates/tui`) and the desktop app
//! (`apps/desktop`). Nothing in this crate deletes, moves or renames anything; the few functions
//! that write a file are listed in `scripts/readonly-allow.txt` and checked in CI.

#![forbid(unsafe_code)]

pub mod dev;
pub mod export;
pub mod import;
pub mod jobs;
pub mod sanitize;
pub mod sessions;
pub mod settings;
pub mod synth;
pub mod table;
pub mod tree;
pub mod walk;

pub use dev::DevView;
pub use table::{Row, Sort, SortKey, TreeView};
pub use tree::{NodeId, Tree};
pub use walk::{walk, Progress, WalkError, WalkOptions};
