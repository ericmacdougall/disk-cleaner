//! `disk-cleaner` command line. `scan` and `plan` are read-only: they never change anything
//! except the output file named with `--out`. `apply` is a dry run unless `--apply` is given;
//! `restore` puts removed paths back without overwriting anything.

mod report;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use disk_cleaner_core::{
    config, plan, read_scan, resolve_active, running_cwds, scan, write_json, ConfigSource,
    FactsConfig, Git, PlanOptions, ScanOptions,
};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(
    name = "disk-cleaner",
    version,
    about = "Understand what fills a disk, plan what to do, and apply the plan safely."
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
    /// Execute a lane plan: back up, recover and remove, re-checking every piece first.
    /// A dry run unless --apply is given.
    Apply(ApplyArgs),
    /// Put a removed path back, byte for byte, from storage. Never overwrites.
    Restore(RestoreArgs),
}

#[derive(Args)]
struct ApplyArgs {
    /// Scan JSON written by `scan --lanes --out`.
    #[arg(long, value_name = "FILE")]
    scan: PathBuf,
    /// Plan JSON written by `plan --json` from that scan.
    #[arg(long, value_name = "FILE")]
    plan: PathBuf,
    /// rclone remote root for backups, e.g. b2-archive2:prunebackup/disk-cleaner-archive.
    /// rclone owns the credentials.
    #[arg(long, value_name = "REMOTE")]
    remote: String,
    /// Act. Without it nothing is written, uploaded, pushed or removed.
    #[arg(long)]
    apply: bool,
    /// Concurrent upload pipelines.
    #[arg(long, default_value_t = 4)]
    jobs: usize,
    /// Shard size bound in MiB (rclone buffers one shard per pipeline in memory).
    #[arg(long, default_value_t = 64)]
    shard_mib: u64,
    /// Deadline in seconds for each rclone command; a hung upload is killed and nothing of it
    /// is removed.
    #[arg(long, default_value_t = 1800)]
    shard_timeout_secs: u64,
    /// Manifest file (default ~/.disk-cleaner/prunedisk/<label>.jsonl).
    #[arg(long, value_name = "FILE")]
    manifest: Option<PathBuf>,
    /// Label used in storage paths and recovery refs (default: the short host name).
    #[arg(long, value_name = "LABEL")]
    host_label: Option<String>,
    /// `upload`: also upload pieces holding secret-named files (default `keep`: they stay local).
    #[arg(long, value_name = "MODE")]
    secrets: Option<String>,
    /// Go ahead although the manifest history in storage has gaps (listed when apply refuses
    /// with status 7). For this run only; the report records it.
    #[arg(long)]
    accept_history_gap: bool,
    /// This state directory takes over the storage label from the state directory that wrote
    /// it last (apply refuses with status 8 otherwise). Only when that one is gone for good.
    #[arg(long)]
    take_over_label: bool,
    /// A remote URL (without credentials) you declare private; recovery refs may be pushed to
    /// it. Repeatable; added to `private_remotes` from the config file.
    #[arg(long = "private-remote", value_name = "URL")]
    private_remotes: Vec<String>,
    /// Config file (default: <platform config dir>/disk-cleaner/config.toml). Apply reads
    /// `private_remotes` and `max_git_admin_bytes` from it.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    /// The rclone binary.
    #[arg(long, default_value = "rclone")]
    rclone: PathBuf,
    /// The git binary (2.32 or later).
    #[arg(long, default_value = "git")]
    git: PathBuf,
    /// The GitHub CLI, used only to confirm that a github.com remote is private before a push.
    #[arg(long, default_value = "gh")]
    gh: PathBuf,
    /// Print the report as JSON.
    #[arg(long)]
    json: bool,
    /// Override the current time (unix seconds), for tests.
    #[arg(long, hide = true)]
    now: Option<i64>,
}

#[derive(Args)]
struct RestoreArgs {
    /// The removed path (a file, or a directory that held removed entries).
    #[arg(long, value_name = "PATH")]
    path: PathBuf,
    /// The rclone remote root the backup was written to.
    #[arg(long, value_name = "REMOTE")]
    remote: String,
    /// Manifest file (default ~/.disk-cleaner/prunedisk/<label>.jsonl).
    #[arg(long, value_name = "FILE")]
    manifest: Option<PathBuf>,
    /// Rebuild the manifest from its copies in storage (when the local one is lost).
    #[arg(long)]
    from_remote: bool,
    /// Label the backup was written with (default: the short host name).
    #[arg(long, value_name = "LABEL")]
    host_label: Option<String>,
    /// With --from-remote: restore from the manifest segments that pass their checks even when
    /// some do not (they are listed, and the exit code is 4).
    #[arg(long)]
    accept_partial: bool,
    /// Restore this run's generation instead of the newest.
    #[arg(long, value_name = "RUN")]
    run: Option<String>,
    /// The rclone binary.
    #[arg(long, default_value = "rclone")]
    rclone: PathBuf,
    /// Print the report as JSON.
    #[arg(long)]
    json: bool,
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
    /// Also split every byte under the roots into bounded pieces and find duplicate content, so
    /// `plan` can give each piece a lane (see docs/SCAN-FORMAT.md, "Pieces and lanes").
    #[arg(long)]
    lanes: bool,
    /// With --lanes: most bytes (logical) in one piece, except a single file or a git node.
    #[arg(
        long,
        value_name = "N",
        default_value_t = disk_cleaner_core::pieces::DEFAULT_MAX_PIECE_BYTES,
        requires = "lanes"
    )]
    max_piece_bytes: u64,
    /// With --lanes: most entries in one piece, except a git node or a secret directory.
    #[arg(
        long,
        value_name = "N",
        default_value_t = disk_cleaner_core::pieces::DEFAULT_MAX_PIECE_FILES,
        requires = "lanes"
    )]
    max_piece_files: u64,
    /// With --lanes: smallest file whose content is compared with others of the same length.
    #[arg(
        long,
        value_name = "N",
        default_value_t = disk_cleaner_core::pieces::DEFAULT_MIN_DUP_BYTES,
        requires = "lanes"
    )]
    min_dup_bytes: u64,
    /// With --lanes: stop hashing file content after this many bytes (default: no limit).
    #[arg(long, value_name = "N", requires = "lanes")]
    max_hash_bytes: Option<u64>,
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
    /// Accept a partial process check: when some running processes cannot be inspected, still
    /// offer removal based on the ones that could. Without this (or the config setting
    /// `trust_partial_process_check = true`) every removing action becomes `ask` then.
    #[arg(long)]
    trust_partial_process_check: bool,
    /// Accept incomplete coverage: when the scan could not read some folders or did not enter
    /// some mount points, still offer repositories for removal after a backup, each reason
    /// saying what was not read (same as the config setting `accept_incomplete_coverage =
    /// true`). Without it every repository's dependents are unknown then, so each one asks.
    #[arg(long)]
    accept_incomplete_coverage: bool,
    /// Retired: strict coverage is now the default. Refused with a message naming
    /// `--accept-incomplete-coverage`.
    #[arg(long, hide = true)]
    require_full_coverage: bool,
    /// Override the current time (unix seconds); for reproducible plans and tests.
    #[arg(long, hide = true)]
    now: Option<i64>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan(a) => run_scan(a),
        Command::Plan(a) => run_plan(a),
        Command::Apply(a) => run_apply(a),
        Command::Restore(a) => run_restore(a),
    }
}

fn run_scan(a: ScanArgs) -> Result<()> {
    let mut opts = ScanOptions::new(a.paths);
    opts.one_file_system = a.one_file_system;
    opts.top_files = a.top_files;
    opts.min_output_bytes = a.min_output_bytes;
    opts.collect_facts = !a.no_git;
    opts.git_jobs = a.git_jobs;
    if a.lanes {
        opts.lanes = Some(disk_cleaner_core::PieceOptions {
            max_piece_bytes: a.max_piece_bytes.max(1),
            max_piece_files: a.max_piece_files.max(1),
            min_dup_bytes: a.min_dup_bytes,
            max_hash_bytes: a.max_hash_bytes,
        });
    }
    let git = Git::new("git", Duration::from_secs(a.git_timeout_secs.max(1)));
    opts.facts = FactsConfig::default().with_git(git);
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
    let (window, window_source) = match (a.active_window_days, source) {
        (Some(n), _) => (n, "--active-window-days".to_string()),
        (None, ConfigSource::File(p)) => {
            (cfg.active_window_days, format!("config {}", p.display()))
        }
        (None, ConfigSource::Default) => (cfg.active_window_days, "default".to_string()),
    };
    let opts = PlanOptions {
        active,
        active_window_days: window,
        window_source,
        now,
        process: running_cwds(),
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

fn state_base() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".disk-cleaner").join("prunedisk"))
}

fn run_apply(a: ApplyArgs) -> Result<()> {
    use disk_cleaner_core::apply::{apply, short_hostname, ApplyOptions};
    let label = a.host_label.unwrap_or_else(short_hostname);
    let secrets_upload = match a.secrets.as_deref() {
        None | Some("keep") => false,
        Some("upload") => true,
        Some(other) => anyhow::bail!("--secrets takes `keep` or `upload`, not {other:?}"),
    };
    let base = state_base()?;
    let manifest = a
        .manifest
        .unwrap_or_else(|| base.join(format!("{label}.jsonl")));
    let now = a.now.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    });
    let scan_text = std::fs::read_to_string(&a.scan)
        .with_context(|| format!("cannot read {}", a.scan.display()))?;
    let plan_text = std::fs::read_to_string(&a.plan)
        .with_context(|| format!("cannot read {}", a.plan.display()))?;
    let (cfg, _) = config::load(a.config.as_deref()).context("invalid configuration")?;
    let mut opts = ApplyOptions::new(a.remote.clone(), manifest, base.join("state"), now);
    opts.oracle = Box::new(disk_cleaner_core::apply::CoreOracle {
        max_git_admin_bytes: cfg.max_git_admin_bytes,
        ..Default::default()
    });
    opts.label = label;
    opts.act = a.apply;
    opts.jobs = a.jobs.max(1);
    opts.shard_bytes = a.shard_mib.max(1) << 20;
    opts.shard_timeout = Duration::from_secs(a.shard_timeout_secs.max(10));
    opts.secrets_upload = secrets_upload;
    opts.accept_history_gap = a.accept_history_gap;
    opts.take_over_label = a.take_over_label;
    opts.private_remotes = cfg.private_remotes;
    opts.private_remotes.extend(a.private_remotes);
    opts.rclone = a.rclone;
    opts.git = a.git;
    opts.gh = a.gh;
    let local = PathBuf::from(&a.remote);
    if local.is_absolute() {
        opts.remote_local = Some(local);
    }
    let res = apply(&scan_text, &plan_text, &opts);
    match &res {
        Ok(rep) if a.json => println!("{}", serde_json::to_string_pretty(rep)?),
        Ok(rep) => print!("{}", report::apply_summary(rep)),
        Err(e) => eprintln!("disk-cleaner apply: {e}"),
    }
    match apply_exit_code(&res) {
        0 => Ok(()),
        code => std::process::exit(code),
    }
}

/// Exit status of `apply`, documented in the README: 0 done; 1 another error; 2 the scan or
/// plan was refused before anything was done; 3 another apply holds the lock; 4 a shard failed,
/// so the pieces depending on it were kept; 5 pieces were kept because an earlier apply run
/// changed their folder, so scan again to continue; 6 the manifest copy in storage is behind; 7
/// the manifest history in storage has gaps, so nothing was done; 8 the storage label belongs
/// to another state directory, so nothing was done. 6 outranks 4, which outranks 5.
fn apply_exit_code(
    res: &Result<disk_cleaner_core::apply::ApplyReport, disk_cleaner_core::apply::ApplyError>,
) -> i32 {
    use disk_cleaner_core::apply::ApplyError;
    match res {
        Err(ApplyError::Plan(_)) => 2,
        Err(ApplyError::Locked(_)) => 3,
        Err(ApplyError::HistoryGap(_)) => 7,
        Err(ApplyError::LabelInUse { .. }) => 8,
        Err(_) => 1,
        Ok(rep) if rep.mirror_failed.is_some() => 6,
        Ok(rep) if !rep.shards_failed.is_empty() => 4,
        Ok(rep) if !rep.rescan_needed.is_empty() => 5,
        Ok(_) => 0,
    }
}

#[cfg(not(unix))]
fn run_restore(_a: RestoreArgs) -> Result<()> {
    anyhow::bail!("restore is not supported on this platform yet")
}

#[cfg(unix)]
fn run_restore(a: RestoreArgs) -> Result<()> {
    use disk_cleaner_core::apply::restore::{restore, RestoreOptions};
    use disk_cleaner_core::apply::short_hostname;
    let label = a.host_label.unwrap_or_else(short_hostname);
    let manifest = match a.manifest {
        Some(m) => m,
        None => state_base()?.join(format!("{label}.jsonl")),
    };
    let opts = RestoreOptions {
        accept_partial: a.accept_partial,
        remote: a.remote,
        rclone: a.rclone,
        git: PathBuf::from("git"),
        manifest,
        from_remote: a.from_remote,
        label,
        run: a.run,
        timeout: Duration::from_secs(1800),
    };
    let rep = restore(&a.path, &opts)?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&rep)?);
    } else {
        print!("{}", report::restore_summary(&rep));
    }
    if !rep.failed.is_empty() || !rep.conflicts.is_empty() || !rep.rejected_segments.is_empty() {
        std::process::exit(4);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::apply_exit_code;
    use disk_cleaner_core::apply::{ApplyError, ApplyReport};
    use std::path::PathBuf;

    #[test]
    fn apply_exit_codes_are_distinct_and_a_failed_shard_outranks_a_stale_scan() {
        assert_eq!(apply_exit_code(&Ok(ApplyReport::default())), 0);
        assert_eq!(apply_exit_code(&Err(ApplyError::Io("x".into()))), 1);
        assert_eq!(apply_exit_code(&Err(ApplyError::Plan("x".into()))), 2);
        assert_eq!(
            apply_exit_code(&Err(ApplyError::Locked(PathBuf::from("/l")))),
            3
        );
        let stale = ApplyReport {
            rescan_needed: vec![1],
            ..Default::default()
        };
        assert_eq!(apply_exit_code(&Ok(stale.clone())), 5);
        let both = ApplyReport {
            shards_failed: vec!["s".into()],
            ..stale
        };
        assert_eq!(apply_exit_code(&Ok(both.clone())), 4);
        // A manifest copy in storage that is behind outranks both.
        let behind = ApplyReport {
            mirror_failed: Some("rcat failed".into()),
            ..both
        };
        assert_eq!(apply_exit_code(&Ok(behind.clone())), 6);
        let s = super::report::apply_summary(&behind);
        assert!(s.contains("run apply again to store it"), "{s}");
        // A history gap refuses the run (A-6), and so does a label of another state directory.
        assert_eq!(
            apply_exit_code(&Err(ApplyError::HistoryGap(vec![
                "a-1.jsonl: missing".into()
            ]))),
            7
        );
        assert_eq!(
            apply_exit_code(&Err(ApplyError::LabelInUse {
                label: "h".into(),
                theirs: "a".into(),
                ours: "b".into()
            })),
            8
        );
        // A run that went ahead with --accept-history-gap says so and lists the gaps.
        let gap = ApplyReport {
            history_gaps: vec!["a-1.jsonl: missing".into()],
            took_over_label_from: Some("0f".repeat(16)),
            rows_from_storage: 3,
            ..Default::default()
        };
        assert_eq!(apply_exit_code(&Ok(gap.clone())), 0);
        let s = super::report::apply_summary(&gap);
        assert!(
            s.contains("went ahead with --accept-history-gap")
                && s.contains("a-1.jsonl: missing")
                && s.contains("--accept-partial")
                && s.contains(&format!(
                    "took over the storage label from state directory {}",
                    "0f".repeat(16)
                ))
                && s.contains("3 manifest row(s) came from the history in storage"),
            "{s}"
        );
        assert!(!s.contains("Stored history was not checked"), "{s}");
        // A dry run never reads storage, and says so (main's ruling: never promise silently
        // more than the acting run does).
        let dry = ApplyReport {
            dry_run: true,
            ..Default::default()
        };
        let s = super::report::apply_summary(&dry);
        assert!(
            s.contains("Stored history was not checked")
                && s.contains("an acting run also reads the history in storage"),
            "{s}"
        );
    }

    #[test]
    fn the_summary_says_to_scan_again() {
        let rep = ApplyReport {
            rescan_needed: vec![0, 2],
            ..Default::default()
        };
        let s = super::report::apply_summary(&rep);
        assert!(
            s.contains(
                "Scan again: 2 piece(s) were kept because an earlier apply run changed this folder."
            ),
            "{s}"
        );
    }
}
