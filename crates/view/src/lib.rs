//! Read-only view model shared by the terminal view (`crates/tui`) and the desktop app
//! (`apps/desktop`). Nothing in this crate deletes, moves or renames anything.

#![forbid(unsafe_code)]

pub mod sanitize;
