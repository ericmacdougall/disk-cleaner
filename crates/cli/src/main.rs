//! `disk-cleaner` command line. Phase 1 is read-only: `scan` and `plan` never change anything
//! except the output file named with `--out`.

mod report;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use disk_cleaner_core::{
    plan, read_scan, scan, write_json, FactsConfig, Git, PlanOptions, ScanOptions,
};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(
    name = "disk-cleaner",
    version,
    about = "Understand what fills a disk. Read-only: scans and dry-run plans only."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan directories, classify them and gather repo safety facts.
    Scan(ScanArgs),
    /// Turn a scan into a dry-run plan. Nothing is changed.
    Plan(PlanArgs),
}

#[derive(Args)]
struct ScanArgs {
    /// Directories to scan.
    #[arg(required = true)]
    paths: Vec<PathBuf>,
    /// Print the scan as JSON on stdout instead of a summary.
    #[arg(long)]
    json: bool,
    /// Also write the scan JSON to this file (the only file written).
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
    /// Do not cross mount points (Unix).
    #[arg(long)]
    one_file_system: bool,
    /// How many of the biggest files to keep.
    #[arg(long, default_value_t = 50)]
    top_files: usize,
    /// List output files (video, archives, models...) of at least this many bytes anywhere.
    #[arg(long, default_value_t = 100 * 1024 * 1024)]
    min_output_bytes: u64,
    /// Skip git; every repo fact is then unknown.
    #[arg(long)]
    no_git: bool,
    /// Per-command git timeout in seconds. A timeout makes the fact unknown.
    #[arg(long, default_value_t = 30)]
    git_timeout_secs: u64,
    /// Number of git processes to run at once.
    #[arg(long, default_value_t = 4)]
    git_jobs: usize,
}

#[derive(Args)]
struct PlanArgs {
    /// Scan JSON produced by `scan --out` or `scan --json`.
    #[arg(long, value_name = "FILE")]
    scan: PathBuf,
    /// A path the user says is active. Repeatable. Everything under it is kept.
    #[arg(long = "active", value_name = "PATH")]
    active: Vec<PathBuf>,
    /// Projects idle at least this many days may be cleaned. There is no default.
    #[arg(long, value_name = "DAYS")]
    idle_days: u64,
    /// Print the plan as JSON.
    #[arg(long)]
    json: bool,
    /// Override the current time (unix seconds); for reproducible plans and tests.
    #[arg(long, hide = true)]
    now: Option<i64>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan(a) => run_scan(a),
        Command::Plan(a) => run_plan(a),
    }
}

fn run_scan(a: ScanArgs) -> Result<()> {
    let mut opts = ScanOptions::new(a.paths);
    opts.one_file_system = a.one_file_system;
    opts.top_files = a.top_files;
    opts.min_output_bytes = a.min_output_bytes;
    opts.collect_facts = !a.no_git;
    opts.git_jobs = a.git_jobs;
    opts.facts = FactsConfig::default().with_git(Git::new(
        "git",
        Duration::from_secs(a.git_timeout_secs.max(1)),
    ));
    let result = scan(&opts).context("scan failed")?;
    if let Some(out) = &a.out {
        write_json(out, &result).context("cannot write scan file")?;
    }
    if a.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        print!("{}", report::scan_summary(&result));
        if let Some(out) = &a.out {
            println!("\nScan written to {}", out.display());
        }
    }
    Ok(())
}

fn run_plan(a: PlanArgs) -> Result<()> {
    let scan_result = read_scan(&a.scan).context("cannot read scan file")?;
    let now = a.now.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    });
    let mut active = Vec::new();
    for p in &a.active {
        active.push(std::path::absolute(p).with_context(|| format!("bad --active {p:?}"))?);
    }
    let opts = PlanOptions {
        active,
        idle_days: a.idle_days,
        now,
    };
    let p = plan(&scan_result, &opts);
    if a.json {
        println!("{}", serde_json::to_string_pretty(&p)?);
    } else {
        print!("{}", report::plan_summary(&p));
    }
    Ok(())
}
