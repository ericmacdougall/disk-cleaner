//! `disk-cleaner` command line. `scan` and `plan` are read-only (they change nothing except the
//! output file named with `--out`). `apply` removes the regenerable directories of a reviewed
//! plan under a journal; it is for a person at their own terminal.

#![deny(unsafe_code)]

mod containers;
mod report;
#[allow(unsafe_code)]
mod sysctx;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use disk_cleaner_core::{
    config, plan, procs, protect, read_scan, resolve_active, scan, write_json, ConfigSource,
    FactsConfig, Git, HostInputs, PlanOptions, ScanContext, ScanOptions,
};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(
    name = "disk-cleaner",
    version,
    about = "Understand what fills a disk. Scans and dry-run plans are read-only; `apply` removes \
             the regenerable directories of a reviewed plan, under a journal"
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
    /// Remove the regenerable directories of a reviewed plan, journalled, or finish an
    /// interrupted run with `--resume`. Meant for a person at their own terminal.
    Apply(ApplyArgs),
}

#[derive(Args)]
struct ApplyArgs {
    /// The reviewed plan (`plan --json`).
    #[arg(long, value_name = "FILE")]
    plan: Option<PathBuf>,
    /// The plan's id (the first 16 hex characters of the SHA-256 of the plan file). Without the
    /// right one nothing is changed: the id and a dry run are printed.
    #[arg(long, value_name = "ID")]
    plan_id: Option<String>,
    /// The lane: only `regenerable` exists.
    #[arg(long, value_name = "LANE")]
    only: Option<String>,
    /// Finish or roll back the unfinished items of this plan id's journal.
    #[arg(long)]
    resume: bool,
    /// With --resume: the plan id, when the plan file is not at hand.
    #[arg(long, value_name = "ID")]
    journal: Option<String>,
    /// Act on at most this many items.
    #[arg(long, value_name = "N")]
    max_items: Option<usize>,
    /// A disposable fixture directory under a temporary root (holding `.prunedisk-sandbox`,
    /// containing every candidate): waives the terminal and agent-marker checks only.
    #[arg(long, value_name = "DIR")]
    sandbox: Option<PathBuf>,
    /// Resolve the home (and its `.prunedisk` state directory) under DIR instead of the passwd
    /// home; HOME must be set to the same directory.
    #[arg(long, value_name = "DIR")]
    home: Option<PathBuf>,
    /// Config file whose `protected_paths` add protected roots (as for `scan`).
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Plans older than this many days are refused.
    #[arg(long, default_value_t = 3)]
    max_plan_age_days: u64,
    /// Read the process snapshot and container state from this JSON file; for tests.
    #[arg(long, hide = true, value_name = "FILE")]
    host_inputs: Option<PathBuf>,
    /// Override the current time (unix seconds); for tests.
    #[arg(long, hide = true)]
    now: Option<i64>,
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
    /// Inside an artifact an output is listed whatever its size, except in an installed
    /// package's directory, where this threshold applies.
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
    /// Resolve the protected agent homes under DIR instead of the passwd home (a disposable
    /// fixture home); HOME must be set to the same directory. The real home stays protected too.
    #[arg(long, value_name = "DIR")]
    home: Option<PathBuf>,
    /// Config file whose `protected_paths` add protected roots (default: <platform config
    /// dir>/disk-cleaner/config.toml). Give `plan` the same file.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

#[derive(Args)]
struct PlanArgs {
    /// Scan JSON produced by `scan --out` or `scan --json`.
    #[arg(long, value_name = "FILE")]
    scan: PathBuf,
    /// A path the user says is active. Repeatable. Everything under it is kept.
    #[arg(long = "active", value_name = "PATH")]
    active: Vec<PathBuf>,
    /// A project is active when touched within this many days (overrides the config file;
    /// the built-in default is 7).
    #[arg(long, value_name = "DAYS")]
    active_window_days: Option<u64>,
    /// Config file (default: <platform config dir>/disk-cleaner/config.toml).
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Print the plan as JSON.
    #[arg(long)]
    json: bool,
    /// Accept processes whose working directory and command line are both unreadable (as
    /// under `hidepid`) as not using anything. Without this (or the config setting
    /// `trust_partial_process_check = true`) every removing action becomes `ask` while such a
    /// process runs. Other processes are judged per item either way.
    #[arg(long)]
    trust_partial_process_check: bool,
    /// Accept an overflowed coverage record: when the scan could not record every place it
    /// could not read, still offer repositories for removal after a backup, each reason saying
    /// what was not read (same as the config setting `accept_incomplete_coverage = true`).
    /// Without it every repository asks then. A complete record is judged per item either way.
    #[arg(long)]
    accept_incomplete_coverage: bool,
    /// Retired: strict coverage is now the default. Refused with a message naming
    /// `--accept-incomplete-coverage`.
    #[arg(long, hide = true)]
    require_full_coverage: bool,
    /// Override the current time (unix seconds); for reproducible plans and tests.
    #[arg(long, hide = true)]
    now: Option<i64>,
    /// Read the process snapshot and container state from this JSON file instead of asking the
    /// system; for reproducible plans and tests on fixture homes.
    #[arg(long, hide = true, value_name = "FILE")]
    host_inputs: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan(a) => run_scan(a),
        Command::Plan(a) => run_plan(a),
        Command::Apply(a) => run_apply(a),
    }
}

fn run_apply(a: ApplyArgs) -> Result<()> {
    use disk_cleaner_core::apply::{self, mounts::HostMounts, Outcome};
    use std::io::IsTerminal;
    let (cfg, _) = config::load(a.config.as_deref()).context("invalid configuration")?;
    let mut ctx = ScanContext::from_env(cfg.protected_paths);
    ctx.euid = sysctx::euid();
    ctx.passwd_home = ctx.euid.and_then(sysctx::passwd_home);
    ctx.declared_home = a.home.clone();
    let home = a
        .home
        .clone()
        .or_else(|| ctx.passwd_home.clone())
        .context("cannot find this user's home directory")?;
    let now = a.now.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    });
    let host = match &a.host_inputs {
        Some(f) => {
            let text = std::fs::read_to_string(f)
                .with_context(|| format!("cannot read {}", f.display()))?;
            serde_json::from_str::<HostInputs>(&text)
                .with_context(|| format!("invalid host inputs in {}", f.display()))?
        }
        None => {
            let processes = procs::snapshot(&sysctx::probe, sysctx::euid());
            let containers = containers::query(&processes);
            HostInputs {
                processes,
                containers,
            }
        }
    };
    // Until the install check and the in-use rules are built in, the guard is unverified and
    // every item is reached: new work refuses, and `--resume` (which needs neither) works.
    let guard = apply::Unverified;
    let reach = apply::ReachUnbuilt;
    let mounts = HostMounts;
    let env = apply::Env {
        state_dir: home.join(".prunedisk"),
        home,
        now,
        max_plan_age_days: a.max_plan_age_days,
        host,
        scan_context: ctx,
        facts: FactsConfig::default().with_git(Git::new("git", Duration::from_secs(30))),
        guard: &guard,
        reach: &reach,
        mounts: &mounts,
    };
    let getenv = |k: &str| std::env::var(k).ok();
    let req = apply::Request {
        plan_file: a.plan.as_deref(),
        plan_id: a.plan_id.as_deref(),
        only: a.only.as_deref(),
        resume: a.resume,
        journal: a.journal.as_deref(),
        max_items: a.max_items,
        sandbox: a.sandbox.as_deref(),
        stdin_is_terminal: std::io::stdin().is_terminal(),
        getenv: &getenv,
    };
    match apply::run(&req, &env).context("apply failed")? {
        Outcome::NothingChanged(text) => {
            print!("{text}");
            Ok(())
        }
        Outcome::Refused(why) => {
            eprintln!("apply refused, nothing changed: {why}");
            std::process::exit(2);
        }
        Outcome::Busy => {
            eprintln!("another apply holds the host lock; nothing changed");
            std::process::exit(3);
        }
        Outcome::Applied(r) | Outcome::Resumed(r) => {
            print!("{r}");
            Ok(())
        }
    }
}

fn run_scan(a: ScanArgs) -> Result<()> {
    let mut opts = ScanOptions::new(a.paths);
    opts.one_file_system = a.one_file_system;
    opts.top_files = a.top_files;
    opts.min_output_bytes = a.min_output_bytes;
    opts.collect_facts = !a.no_git;
    opts.git_jobs = a.git_jobs;
    let git = Git::new("git", Duration::from_secs(a.git_timeout_secs.max(1)));
    opts.facts = FactsConfig::default().with_git(git);
    // Who is running and where agent data lives: asked of the system here, passed to the core.
    // The config's `protected_paths` add roots: from `--config FILE`, or the default location.
    // `plan` refuses entries this scan did not resolve, so the two must read the same file.
    let (cfg, _) = config::load(a.config.as_deref()).context("invalid configuration")?;
    let mut ctx = ScanContext::from_env(cfg.protected_paths);
    ctx.euid = sysctx::euid();
    ctx.passwd_home = ctx.euid.and_then(sysctx::passwd_home);
    if ctx.euid.is_some() && ctx.passwd_home.is_none() {
        anyhow::bail!(
            "cannot read this user's home directory from the passwd database, so the agent \
             homes to protect cannot be located; refusing to scan"
        );
    }
    ctx.declared_home = a.home;
    opts.context = ctx;
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
    if a.require_full_coverage {
        anyhow::bail!(
            "--require-full-coverage is no longer an option: incomplete coverage now makes \
             repositories ask by default; drop the flag, or pass --accept-incomplete-coverage \
             (config: accept_incomplete_coverage = true) to offer them for removal despite \
             folders the scan could not read"
        );
    }
    let scan_result = read_scan(&a.scan).context("cannot read scan file")?;
    // The plan trusts the scan's protected roots only when they are there and are this user's.
    protect::check_plan_scan(&scan_result, sysctx::euid()).context("scan file not accepted")?;
    let now = a.now.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    });
    let active = resolve_active(&scan_result, &a.active)?;
    // The config is always loaded and validated, even when a flag overrides it, so a broken
    // file is reported instead of being ignored.
    let (cfg, source) = config::load(a.config.as_deref()).context("invalid configuration")?;
    // Roots are resolved by the scan: an entry it did not protect is refused, never ignored.
    protect::check_config_paths(&scan_result, &cfg.protected_paths)
        .context("configuration not covered by the scan")?;
    let (window, window_source) = match (a.active_window_days, source) {
        (Some(n), _) => (n, "--active-window-days".to_string()),
        (None, ConfigSource::File(p)) => {
            (cfg.active_window_days, format!("config {}", p.display()))
        }
        (None, ConfigSource::Default) => (cfg.active_window_days, "default".to_string()),
    };
    let host = match &a.host_inputs {
        Some(f) => {
            let text = std::fs::read_to_string(f)
                .with_context(|| format!("cannot read {}", f.display()))?;
            serde_json::from_str::<HostInputs>(&text)
                .with_context(|| format!("invalid host inputs in {}", f.display()))?
        }
        None => {
            let processes = procs::snapshot(&sysctx::probe, sysctx::euid());
            let containers = containers::query(&processes);
            HostInputs {
                processes,
                containers,
            }
        }
    };
    let opts = PlanOptions {
        active,
        active_window_days: window,
        window_source,
        now,
        process: host.processes,
        containers: host.containers,
        trust_partial_process_check: a.trust_partial_process_check
            || cfg.trust_partial_process_check,
        accept_incomplete_coverage: a.accept_incomplete_coverage || cfg.accept_incomplete_coverage,
        max_git_admin_bytes: cfg.max_git_admin_bytes,
    };
    let p = plan(&scan_result, &opts);
    if a.json {
        println!("{}", serde_json::to_string_pretty(&p)?);
    } else {
        print!("{}", report::plan_summary(&p));
    }
    Ok(())
}
