//! `disk-cleaner tui`: a read-only terminal view of a scan.

#![forbid(unsafe_code)]

use clap::Args;
use std::path::PathBuf;

/// Arguments of `disk-cleaner tui`.
#[derive(Args, Debug, Clone)]
pub struct TuiArgs {
    /// Folders to scan and browse.
    pub paths: Vec<PathBuf>,
    /// Browse a scan file written by `disk-cleaner scan --out` instead of scanning.
    #[arg(long, value_name = "FILE", conflicts_with = "paths")]
    pub scan: Option<PathBuf>,
}

/// Run the terminal view.
pub fn run(args: TuiArgs) -> anyhow::Result<()> {
    if args.paths.is_empty() && args.scan.is_none() {
        anyhow::bail!("give one or more folders to scan, or --scan FILE");
    }
    anyhow::bail!("the terminal view is not built yet")
}
